//! Play state: the 20 TPS loop that answers keep-alives, confirms teleports
//! and chunk batches, and maintains local player and entity state from the
//! authoritative clientbound packets.
//!
//! Response-bearing packets (keep-alive, position, chunk batch, disconnect)
//! are handled inline. State-only packets (login, health, experience and the
//! entity movement family) are decoded and folded into [`PlayState`] by
//! [`apply_state_packet`]; they require no wire response. Anything still
//! unclassified is logged per the coverage table.
//!
//! Packet ids and layouts come from the generated protocol
//! (`minerider_protocol::generated::v1_21_4::play`).

use minerider_protocol::buffer::{PacketReader, PacketWriter};
use minerider_protocol::generated::v1_21_4::play::{
    PacketChunkBatchFinished, PacketChunkBatchReceived, PacketEntityDestroy,
    PacketEntityHeadRotation, PacketEntityLook, PacketEntityMoveLook, PacketEntityTeleport,
    PacketEntityVelocity, PacketExperience, PacketKeepAlive, PacketKickDisconnect, PacketLogin,
    PacketPosition, PacketRelEntityMove, PacketSpawnEntity, PacketSyncEntityPosition,
    PacketTeleportConfirm, PacketUpdateHealth, CLIENTBOUND_CHUNK_BATCH_FINISHED_ID,
    CLIENTBOUND_ENTITY_DESTROY_ID, CLIENTBOUND_ENTITY_HEAD_ROTATION_ID, CLIENTBOUND_ENTITY_LOOK_ID,
    CLIENTBOUND_ENTITY_MOVE_LOOK_ID, CLIENTBOUND_ENTITY_TELEPORT_ID,
    CLIENTBOUND_ENTITY_VELOCITY_ID, CLIENTBOUND_EXPERIENCE_ID, CLIENTBOUND_KEEP_ALIVE_ID,
    CLIENTBOUND_KICK_DISCONNECT_ID, CLIENTBOUND_LOGIN_ID, CLIENTBOUND_POSITION_ID,
    CLIENTBOUND_REL_ENTITY_MOVE_ID, CLIENTBOUND_SPAWN_ENTITY_ID,
    CLIENTBOUND_SYNC_ENTITY_POSITION_ID, CLIENTBOUND_UPDATE_HEALTH_ID,
    SERVERBOUND_CHUNK_BATCH_RECEIVED_ID, SERVERBOUND_KEEP_ALIVE_ID,
    SERVERBOUND_TELEPORT_CONFIRM_ID,
};
use minerider_protocol::packet::RawPacket;
use minerider_protocol::traits::{Decode, Encode};
use tracing::{debug, warn};

use crate::core::error::{MineRiderError, Result};
use crate::core::state::ConnectionState;
use crate::core::tick::{TickClock, TICK_DURATION};
use crate::minecraft::coverage::{clientbound_coverage, CoverageClass};
use crate::minecraft::entity::EntityStore;
use crate::minecraft::nbt_reason_text;
use crate::minecraft::player::LocalPlayer;
use crate::network::connection::Connection;

/// Aggregate play-state: the tick counter plus local player and entity state.
#[derive(Debug, Default)]
pub struct PlayState {
    /// Own player state (position, vitals, experience).
    pub player: LocalPlayer,
    /// Tracked non-local entities.
    pub entities: EntityStore,
    /// Monotonic 20 TPS tick counter.
    pub clock: TickClock,
}

impl PlayState {
    /// Fresh play state for a newly-joined client.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Runs the play-state loop: a `select!` between the packet stream and a
/// 20 TPS tick. Answers keep-alives, confirms teleports, acknowledges chunk
/// batches, folds state-only packets into [`PlayState`], reports
/// disconnects, and logs everything else per the coverage table. Returns
/// only on error/disconnect.
pub async fn run_play(conn: &mut Connection) -> Result<()> {
    let mut state = PlayState::new();
    // High-frequency ignored packets warn once per id, then drop to debug;
    // bounded by the number of clientbound play ids, so memory is fixed.
    let mut warned_ids = std::collections::HashSet::new();

    let mut ticker = tokio::time::interval(TICK_DURATION);
    // A long send or scheduler hiccup must not trigger a catch-up burst of
    // ticks; skip missed ticks and resume the cadence.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            // Prefer draining the packet stream over advancing the tick.
            biased;
            read = conn.read_packet() => {
                let packet = read?;
                handle_clientbound(conn, &mut state, &packet, &mut warned_ids).await?;
            }
            _ = ticker.tick() => {
                state.clock.advance();
            }
        }
    }
}

/// Handles one clientbound play packet: sends any required response and
/// updates [`PlayState`].
async fn handle_clientbound(
    conn: &mut Connection,
    state: &mut PlayState,
    packet: &RawPacket,
    warned_ids: &mut std::collections::HashSet<i32>,
) -> Result<()> {
    match packet.id {
        CLIENTBOUND_KEEP_ALIVE_ID => {
            let mut r = PacketReader::new(&packet.payload);
            let keep_alive = PacketKeepAlive::decode(&mut r)?;
            let mut w = PacketWriter::new();
            keep_alive.encode(&mut w)?;
            conn.send_packet(SERVERBOUND_KEEP_ALIVE_ID, &w.freeze())
                .await?;
        }
        CLIENTBOUND_POSITION_ID => {
            let mut r = PacketReader::new(&packet.payload);
            let sync = PacketPosition::decode(&mut r)?;
            state.player.on_position(&sync);
            let confirm = PacketTeleportConfirm {
                teleport_id: sync.teleport_id,
            };
            let mut w = PacketWriter::new();
            confirm.encode(&mut w)?;
            conn.send_packet(SERVERBOUND_TELEPORT_CONFIRM_ID, &w.freeze())
                .await?;
            debug!(
                x = state.player.position.x,
                y = state.player.position.y,
                z = state.player.position.z,
                teleport_id = sync.teleport_id,
                "confirmed teleport"
            );
        }
        CLIENTBOUND_CHUNK_BATCH_FINISHED_ID => {
            let mut r = PacketReader::new(&packet.payload);
            let finished = PacketChunkBatchFinished::decode(&mut r)?;
            // Vanilla reports the desired chunks-per-tick rate; with no chunk
            // processing of our own yet, the batch size is the correct
            // initial value (we acknowledged it immediately).
            let ack = PacketChunkBatchReceived {
                chunks_per_tick: finished.batch_size as f32,
            };
            let mut w = PacketWriter::new();
            ack.encode(&mut w)?;
            conn.send_packet(SERVERBOUND_CHUNK_BATCH_RECEIVED_ID, &w.freeze())
                .await?;
            debug!(batch_size = finished.batch_size, "acknowledged chunk batch");
        }
        CLIENTBOUND_KICK_DISCONNECT_ID => {
            let mut r = PacketReader::new(&packet.payload);
            let disconnect = PacketKickDisconnect::decode(&mut r)?;
            return Err(MineRiderError::Disconnected(nbt_reason_text(
                &disconnect.reason,
            )));
        }
        other => {
            if apply_state_packet(state, other, &packet.payload)? {
                return Ok(());
            }
            log_unhandled(other, packet.payload.len(), warned_ids);
        }
    }
    Ok(())
}

/// Decodes and folds a state-only clientbound packet into [`PlayState`].
///
/// Returns `Ok(true)` if `id` is one of the tracked state packets (login,
/// vitals, experience, entity movement family), `Ok(false)` otherwise. These
/// packets carry no wire response obligation.
fn apply_state_packet(state: &mut PlayState, id: i32, payload: &[u8]) -> Result<bool> {
    let mut r = PacketReader::new(payload);
    match id {
        CLIENTBOUND_LOGIN_ID => {
            let p = PacketLogin::decode(&mut r)?;
            state.player.on_login(&p);
            debug!(
                entity_id = p.entity_id,
                "play login: own entity id recorded"
            );
        }
        CLIENTBOUND_UPDATE_HEALTH_ID => {
            let p = PacketUpdateHealth::decode(&mut r)?;
            state.player.on_health(&p);
        }
        CLIENTBOUND_EXPERIENCE_ID => {
            let p = PacketExperience::decode(&mut r)?;
            state.player.on_experience(&p);
        }
        CLIENTBOUND_SPAWN_ENTITY_ID => {
            let p = PacketSpawnEntity::decode(&mut r)?;
            state.entities.spawn(&p);
        }
        CLIENTBOUND_ENTITY_DESTROY_ID => {
            let p = PacketEntityDestroy::decode(&mut r)?;
            state.entities.destroy(&p.entity_ids);
        }
        CLIENTBOUND_REL_ENTITY_MOVE_ID => {
            let p = PacketRelEntityMove::decode(&mut r)?;
            state.entities.rel_move(&p);
        }
        CLIENTBOUND_ENTITY_MOVE_LOOK_ID => {
            let p = PacketEntityMoveLook::decode(&mut r)?;
            state.entities.move_look(&p);
        }
        CLIENTBOUND_ENTITY_LOOK_ID => {
            let p = PacketEntityLook::decode(&mut r)?;
            state.entities.look(&p);
        }
        CLIENTBOUND_ENTITY_TELEPORT_ID => {
            let p = PacketEntityTeleport::decode(&mut r)?;
            state.entities.teleport(&p);
        }
        CLIENTBOUND_SYNC_ENTITY_POSITION_ID => {
            let p = PacketSyncEntityPosition::decode(&mut r)?;
            state.entities.sync_position(&p);
        }
        CLIENTBOUND_ENTITY_VELOCITY_ID => {
            let p = PacketEntityVelocity::decode(&mut r)?;
            state.entities.velocity(&p);
        }
        CLIENTBOUND_ENTITY_HEAD_ROTATION_ID => {
            let p = PacketEntityHeadRotation::decode(&mut r)?;
            state.entities.head_rotation(&p);
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// Logs a clientbound play packet that has no handler, per its coverage
/// class: `IntentionallyIgnored`/`StoredForLater` warn once then drop to
/// debug; `Unsupported` warns every time; `Handled` (already dispatched
/// elsewhere) logs at debug.
fn log_unhandled(id: i32, len: usize, warned_ids: &mut std::collections::HashSet<i32>) {
    let entry = clientbound_coverage(ConnectionState::Play, id);
    match entry.class {
        CoverageClass::Handled => {
            debug!(id = format_args!("0x{id:02x}"), len, "ignoring play packet")
        }
        CoverageClass::IntentionallyIgnored | CoverageClass::StoredForLater => {
            if warned_ids.insert(id) {
                warn!(
                    id = format_args!("0x{id:02x}"),
                    len,
                    coverage = ?entry.class,
                    status = ?entry.obligation.status,
                    "play packet decoded but not handled (further occurrences at debug)"
                );
            } else {
                debug!(
                    id = format_args!("0x{id:02x}"),
                    len, "play packet decoded but not handled"
                );
            }
        }
        CoverageClass::Unsupported => warn!(
            id = format_args!("0x{id:02x}"),
            len,
            status = ?entry.obligation.status,
            "UNSUPPORTED play packet; server may expect a response"
        ),
    }
}
