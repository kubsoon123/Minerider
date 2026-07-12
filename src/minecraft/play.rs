//! Play state entry point: keep-alive echoes, teleport confirmations,
//! chunk batch acknowledgements. Full behavior in phase 3.
//!
//! Packet ids and layouts come from the generated protocol
//! (`minerider_protocol::generated::v1_21_4::play`).

use minerider_protocol::buffer::{PacketReader, PacketWriter};
use minerider_protocol::generated::v1_21_4::play::{
    PacketChunkBatchFinished, PacketChunkBatchReceived, PacketKeepAlive, PacketKickDisconnect,
    PacketPosition, PacketTeleportConfirm, PositionUpdateRelatives,
    CLIENTBOUND_CHUNK_BATCH_FINISHED_ID, CLIENTBOUND_KEEP_ALIVE_ID, CLIENTBOUND_KICK_DISCONNECT_ID,
    CLIENTBOUND_POSITION_ID, SERVERBOUND_CHUNK_BATCH_RECEIVED_ID, SERVERBOUND_KEEP_ALIVE_ID,
    SERVERBOUND_TELEPORT_CONFIRM_ID,
};
use minerider_protocol::traits::{Decode, Encode};
use tracing::{debug, warn};

use crate::core::error::{MineRiderError, Result};
use crate::core::state::ConnectionState;
use crate::minecraft::coverage::{clientbound_coverage, CoverageClass};
use crate::minecraft::nbt_reason_text;
use crate::network::connection::Connection;

/// The client's current position and rotation as confirmed by the server.
///
/// Updated only from `synchronize_player_position`; relative flag bits
/// are applied against the previous value, exactly as vanilla does.
/// Velocity and physics belong to phase 3.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayerPosition {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub yaw: f32,
    pub pitch: f32,
}

impl PlayerPosition {
    const ORIGIN: PlayerPosition = PlayerPosition {
        x: 0.0,
        y: 0.0,
        z: 0.0,
        yaw: 0.0,
        pitch: 0.0,
    };

    /// Applies a `synchronize_player_position` packet, honoring the
    /// relative-flag bitmask (`PositionUpdateRelatives`).
    fn apply(&mut self, packet: &PacketPosition) {
        let flags = packet.flags.0;
        if flags & PositionUpdateRelatives::X != 0 {
            self.x += packet.x;
        } else {
            self.x = packet.x;
        }
        if flags & PositionUpdateRelatives::Y != 0 {
            self.y += packet.y;
        } else {
            self.y = packet.y;
        }
        if flags & PositionUpdateRelatives::Z != 0 {
            self.z += packet.z;
        } else {
            self.z = packet.z;
        }
        if flags & PositionUpdateRelatives::YAW != 0 {
            self.yaw += packet.yaw;
        } else {
            self.yaw = packet.yaw;
        }
        if flags & PositionUpdateRelatives::PITCH != 0 {
            self.pitch += packet.pitch;
        } else {
            self.pitch = packet.pitch;
        }
    }
}

/// Runs the minimal play-state loop: answers keep-alives, confirms
/// teleports, acknowledges chunk batches, reports disconnects, and logs
/// everything else per the coverage table. Returns only on
/// error/disconnect.
pub async fn run_play(conn: &mut Connection) -> Result<()> {
    let mut position = PlayerPosition::ORIGIN;
    loop {
        let packet = conn.read_packet().await?;
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
                position.apply(&sync);
                let confirm = PacketTeleportConfirm {
                    teleport_id: sync.teleport_id,
                };
                let mut w = PacketWriter::new();
                confirm.encode(&mut w)?;
                conn.send_packet(SERVERBOUND_TELEPORT_CONFIRM_ID, &w.freeze())
                    .await?;
                debug!(
                    x = position.x,
                    y = position.y,
                    z = position.z,
                    teleport_id = sync.teleport_id,
                    "confirmed teleport"
                );
            }
            CLIENTBOUND_CHUNK_BATCH_FINISHED_ID => {
                let mut r = PacketReader::new(&packet.payload);
                let finished = PacketChunkBatchFinished::decode(&mut r)?;
                // Vanilla reports the desired chunks-per-tick rate; with no
                // chunk processing of our own yet, the batch size is the
                // correct initial value (we acknowledged it immediately).
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
                let entry = clientbound_coverage(ConnectionState::Play, other);
                match entry.class {
                    CoverageClass::Handled => debug!(
                        id = format_args!("0x{other:02x}"),
                        len = packet.payload.len(),
                        "ignoring play packet"
                    ),
                    CoverageClass::IntentionallyIgnored | CoverageClass::StoredForLater => warn!(
                        id = format_args!("0x{other:02x}"),
                        len = packet.payload.len(),
                        coverage = ?entry.class,
                        status = ?entry.obligation.status,
                        "play packet decoded but not handled"
                    ),
                    CoverageClass::Unsupported => warn!(
                        id = format_args!("0x{other:02x}"),
                        len = packet.payload.len(),
                        status = ?entry.obligation.status,
                        "UNSUPPORTED play packet; server may expect a response"
                    ),
                }
            }
        }
    }
}
