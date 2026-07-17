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
    PacketBlockChange, PacketChatCommand, PacketChatMessage, PacketChunkBatchFinished,
    PacketChunkBatchReceived, PacketClientCommand, PacketCloseWindow, PacketCraftProgressBar,
    PacketEntityDestroy, PacketEntityHeadRotation, PacketEntityLook, PacketEntityMoveLook,
    PacketEntityTeleport, PacketEntityVelocity, PacketExperience, PacketGameStateChange,
    PacketHeldItemSlot, PacketKeepAlive, PacketLogin, PacketMapChunk, PacketMultiBlockChange,
    PacketOpenWindow, PacketPlayerInfo, PacketPlayerRemove, PacketPosition, PacketRelEntityMove,
    PacketResourcePackReceive, PacketRespawn, PacketSetCursorItem, PacketSetSlot,
    PacketSpawnEntity, PacketSyncEntityPosition, PacketTeleportConfirm, PacketUnloadChunk,
    PacketUpdateHealth, PacketUpdateTime, PacketWindowItems, CLIENTBOUND_ADD_RESOURCE_PACK_ID,
    CLIENTBOUND_BLOCK_CHANGE_ID, CLIENTBOUND_CHUNK_BATCH_FINISHED_ID, CLIENTBOUND_CLOSE_WINDOW_ID,
    CLIENTBOUND_CRAFT_PROGRESS_BAR_ID, CLIENTBOUND_ENTITY_DESTROY_ID,
    CLIENTBOUND_ENTITY_HEAD_ROTATION_ID, CLIENTBOUND_ENTITY_LOOK_ID,
    CLIENTBOUND_ENTITY_MOVE_LOOK_ID, CLIENTBOUND_ENTITY_TELEPORT_ID,
    CLIENTBOUND_ENTITY_VELOCITY_ID, CLIENTBOUND_EXPERIENCE_ID, CLIENTBOUND_GAME_STATE_CHANGE_ID,
    CLIENTBOUND_HELD_ITEM_SLOT_ID, CLIENTBOUND_KEEP_ALIVE_ID, CLIENTBOUND_KICK_DISCONNECT_ID,
    CLIENTBOUND_LOGIN_ID, CLIENTBOUND_MAP_CHUNK_ID, CLIENTBOUND_MULTI_BLOCK_CHANGE_ID,
    CLIENTBOUND_OPEN_WINDOW_ID, CLIENTBOUND_PLAYER_INFO_ID, CLIENTBOUND_PLAYER_REMOVE_ID,
    CLIENTBOUND_POSITION_ID, CLIENTBOUND_REL_ENTITY_MOVE_ID, CLIENTBOUND_REMOVE_RESOURCE_PACK_ID,
    CLIENTBOUND_RESPAWN_ID, CLIENTBOUND_SET_CURSOR_ITEM_ID, CLIENTBOUND_SET_SLOT_ID,
    CLIENTBOUND_SPAWN_ENTITY_ID, CLIENTBOUND_SYNC_ENTITY_POSITION_ID, CLIENTBOUND_UNLOAD_CHUNK_ID,
    CLIENTBOUND_UPDATE_HEALTH_ID, CLIENTBOUND_UPDATE_TIME_ID, CLIENTBOUND_WINDOW_ITEMS_ID,
    SERVERBOUND_CHAT_COMMAND_ID, SERVERBOUND_CHAT_MESSAGE_ID, SERVERBOUND_CHUNK_BATCH_RECEIVED_ID,
    SERVERBOUND_CLIENT_COMMAND_ID, SERVERBOUND_FLYING_ID, SERVERBOUND_KEEP_ALIVE_ID,
    SERVERBOUND_LOOK_ID, SERVERBOUND_PLAYER_LOADED_ID, SERVERBOUND_POSITION_ID,
    SERVERBOUND_POSITION_LOOK_ID, SERVERBOUND_RESOURCE_PACK_RECEIVE_ID,
    SERVERBOUND_TELEPORT_CONFIRM_ID,
};
use minerider_protocol::generated::v1_21_4::types::PacketCommonAddResourcePack;
use minerider_protocol::packet::RawPacket;
use minerider_protocol::traits::{Decode, Encode};
use tracing::{debug, warn};

use tokio::sync::broadcast;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::watch;

use crate::core::error::{MineRiderError, Result};
use crate::core::state::ConnectionState;
use crate::core::tick::{TickClock, TICK_DURATION};
use crate::minecraft::configuration::{ConfigurationData, DimensionType};
use crate::minecraft::control::{BotCommand, Controller};
use crate::minecraft::coverage::{clientbound_coverage, CoverageClass};
use crate::minecraft::entity::EntityStore;
use crate::minecraft::event::BotEvent;
use crate::minecraft::inventory::InventoryState;
use crate::minecraft::physics::Vec3;
use crate::minecraft::player::{LocalPlayer, MovementPacket};
use crate::minecraft::players::PlayerList;
use crate::minecraft::presentation::{
    decode_update, ChatKind, PresentationEvent, PresentationState,
};
use crate::minecraft::world::World;
use crate::minecraft::RESOURCE_PACK_STATUS_DECLINED;

/// Vanilla `game_state_change` reasons this client acts on. Values match the
/// declaration order of `ClientboundGameEventPacket.Type` in the 1.21.4
/// client (`START_RAINING = 1`, `STOP_RAINING = 2`, `CHANGE_GAME_MODE = 3`),
/// verified against the vanilla source — getting begin/end raining swapped is
/// a silent weather-state bug.
const GAME_STATE_BEGIN_RAINING: u8 = 1;
const GAME_STATE_END_RAINING: u8 = 2;
const GAME_STATE_CHANGE_GAMEMODE: u8 = 3;
use crate::network::connection::Connection;

/// Aggregate play-state: the tick counter plus local player and entity state.
#[derive(Debug)]
pub struct PlayState {
    /// Own player state (position, vitals, experience).
    pub player: LocalPlayer,
    /// Tracked non-local entities.
    pub entities: EntityStore,
    /// Own inventory, any open container, and the cursor item.
    pub inventory: InventoryState,
    /// Tab-list players, keyed by uuid.
    pub players: PlayerList,
    /// Chat, titles, action bar, tab-list header/footer, and boss bars.
    pub presentation: PresentationState,
    /// World time (day-time in ticks) and whether it is raining.
    pub world_time: i64,
    pub raining: bool,
    /// Per-tick movement driver fed by the control channel.
    pub controller: Controller,
    /// Monotonic 20 TPS tick counter.
    pub clock: TickClock,
    /// Broadcast sink for observable events; see [`crate::minecraft::event`].
    event_tx: broadcast::Sender<BotEvent>,
    received_position: bool,
    dimension: Option<DimensionType>,
    world: Option<World>,
    /// Whether a `client_command` respawn request is outstanding: set when we
    /// detect death, cleared when the `respawn` packet arrives. Prevents
    /// resending the request every tick while the server processes it.
    respawn_pending: bool,
    /// Whether the bot was alive on the previous vitals update, so a death
    /// edge fires the `Death` event exactly once.
    was_alive: bool,
}

/// Vanilla `ClientCommandPacket.Action.PERFORM_RESPAWN`: click the death
/// screen's Respawn button. A bot has no screen to click, so it sends this
/// itself as soon as it detects it has died, mirroring the auto-respawn
/// behavior standard in bot frameworks like Mineflayer.
const RESPAWN_ACTION_ID: i32 = 0;

impl PlayState {
    /// Fresh play state for a newly-joined client, emitting events into
    /// `event_tx`.
    pub fn new(event_tx: broadcast::Sender<BotEvent>) -> Self {
        Self {
            player: LocalPlayer::default(),
            entities: EntityStore::default(),
            inventory: InventoryState::default(),
            players: PlayerList::default(),
            presentation: PresentationState::default(),
            world_time: 0,
            raining: false,
            controller: Controller::default(),
            clock: TickClock::default(),
            event_tx,
            received_position: false,
            dimension: None,
            world: None,
            respawn_pending: false,
            was_alive: true,
        }
    }

    /// Broadcasts an event to all subscribers; a full/closed channel is
    /// ignored (observers must never back-pressure the play loop).
    fn emit(&self, event: BotEvent) {
        let _ = self.event_tx.send(event);
    }

    fn apply_presentation(&mut self, update: crate::minecraft::presentation::PresentationUpdate) {
        let event = self.presentation.apply(update);
        self.emit(BotEvent::Presentation(event.clone()));
        match event {
            PresentationEvent::Chat(message) => match message.kind {
                ChatKind::Player | ChatKind::Disguised => {
                    let sender = message
                        .sender
                        .as_ref()
                        .map(|value| value.plain_text())
                        .filter(|value| !value.is_empty())
                        .or_else(|| {
                            message
                                .signed
                                .as_ref()
                                .map(|signed| signed.sender_uuid.to_string())
                        })
                        .unwrap_or_default();
                    self.emit(BotEvent::Chat {
                        sender,
                        message: message.content.plain_text(),
                    });
                }
                ChatKind::System => self.emit(BotEvent::SystemChat {
                    message: message.content.plain_text(),
                }),
            },
            PresentationEvent::Disconnected { reason } => self.emit(BotEvent::Kicked {
                reason: reason.plain_text(),
            }),
            _ => {}
        }
    }

    /// A cheap, externally-readable snapshot of the parts of play state a
    /// caller would want to observe (position, health, inventory, entities).
    /// The full `World` (block/chunk data) is intentionally excluded: cloning
    /// it every tick would be expensive and callers needing block queries can
    /// be served by a future on-demand accessor instead.
    pub fn snapshot(&self, tick: u64) -> StateSnapshot {
        StateSnapshot {
            tick,
            player: self.player.clone(),
            entities: self.entities.clone(),
            inventory: self.inventory.clone(),
            players: self.players.clone(),
            presentation: self.presentation.clone(),
            world_time: self.world_time,
            raining: self.raining,
        }
    }
}

/// A read-only view of play state published once per tick (and after every
/// clientbound packet) over a [`watch`] channel, the read-side counterpart to
/// [`crate::minecraft::control::ControlHandle`]. External code — a scripting
/// layer, a CLI status line, a test — reads this instead of reaching into the
/// play loop's internals, which it cannot do: the loop owns its state on its
/// own task.
#[derive(Debug, Clone, Default)]
pub struct StateSnapshot {
    /// The tick this snapshot was published on.
    pub tick: u64,
    pub player: LocalPlayer,
    pub entities: EntityStore,
    pub inventory: InventoryState,
    pub players: PlayerList,
    pub presentation: PresentationState,
    pub world_time: i64,
    pub raining: bool,
}

/// Runs the play-state loop: a `select!` between the packet stream and a
/// 20 TPS tick. Answers keep-alives, confirms teleports, acknowledges chunk
/// batches, folds state-only packets into [`PlayState`], reports
/// disconnects, and logs everything else per the coverage table. Returns
/// only on error/disconnect.
pub async fn run_play(
    conn: &mut Connection,
    configuration: &ConfigurationData,
    mut control_rx: UnboundedReceiver<BotCommand>,
    state_tx: watch::Sender<StateSnapshot>,
    event_tx: broadcast::Sender<BotEvent>,
) -> Result<()> {
    let mut state = PlayState::new(event_tx);
    // High-frequency ignored packets warn once per id, then drop to debug;
    // bounded by the number of clientbound play ids, so memory is fixed.
    let mut warned_ids = std::collections::HashSet::new();
    // Once every control handle is dropped the channel closes; disable its
    // select branch so a permanently-ready `recv` cannot spin the loop.
    let mut control_open = true;

    let mut ticker =
        tokio::time::interval_at(tokio::time::Instant::now() + TICK_DURATION, TICK_DURATION);
    // A long send or scheduler hiccup must not trigger a catch-up burst of
    // ticks; skip missed ticks and resume the cadence.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            // Prefer draining the packet stream over advancing the tick.
            biased;
            read = conn.read_packet() => {
                let packet = read?;
                let result = handle_clientbound(
                    conn,
                    &mut state,
                    configuration,
                    &packet,
                    &mut warned_ids,
                ).await;
                // Publish promptly on state-changing packets (health, death,
                // inventory, presentation, entities) rather than waiting up
                // to one tick. Publish disconnect state before returning its
                // terminal error as well.
                let _ = state_tx.send(state.snapshot(state.clock.current()));
                result?;
            }
            command = control_rx.recv(), if control_open => {
                match command {
                    // Chat is the one command that goes straight to the wire
                    // rather than into the controller's local state.
                    Some(BotCommand::Chat(text)) => send_chat(conn, &text).await?,
                    Some(command) => apply_command(&mut state, command),
                    None => control_open = false,
                }
            }
            _ = ticker.tick() => {
                state.clock.advance();
                handle_tick(conn, &mut state).await?;
                let _ = state_tx.send(state.snapshot(state.clock.current()));
            }
        }
    }
}

/// Applies one bot command to the play state. `Look` acts directly on the
/// player's facing; everything else updates the controller.
fn apply_command(state: &mut PlayState, command: BotCommand) {
    match command {
        BotCommand::Look { yaw, pitch } => {
            state.controller.apply_look(&mut state.player, yaw, pitch)
        }
        other => state.controller.apply(other),
    }
}

/// Sends chat: a leading `/` becomes a `chat_command`, anything else an
/// unsigned `chat_message`.
///
/// The message is sent unsigned (no cryptographic signature). Offline-mode
/// servers and servers with `enforce-secure-profile=false` accept this;
/// servers that enforce secure chat will reject or kick unsigned messages —
/// full message signing (a per-message ECDSA signature over the chat session
/// key from `/player/certificates`) is a deliberate follow-up.
async fn send_chat(conn: &mut Connection, text: &str) -> Result<()> {
    if let Some(command) = text.strip_prefix('/') {
        let packet = PacketChatCommand {
            command: command.to_string(),
        };
        let mut w = PacketWriter::new();
        packet.encode(&mut w)?;
        conn.send_packet(SERVERBOUND_CHAT_COMMAND_ID, &w.freeze())
            .await?;
        debug!(%command, "sent command");
        return Ok(());
    }

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let packet = PacketChatMessage {
        message: text.to_string(),
        timestamp,
        salt: rand::random(),
        signature: None,
        // No message-chain acknowledgement is tracked, so acknowledge zero
        // prior messages: offset 0 and an empty (all-zero) 3-byte bitset.
        offset: 0,
        acknowledged: vec![0u8; 3],
    };
    let mut w = PacketWriter::new();
    packet.encode(&mut w)?;
    conn.send_packet(SERVERBOUND_CHAT_MESSAGE_ID, &w.freeze())
        .await?;
    debug!(message = %text, "sent chat");
    Ok(())
}

/// Runs vanilla's per-tick play-entry and movement behavior.
async fn handle_tick(conn: &mut Connection, state: &mut PlayState) -> Result<()> {
    if !state.player.is_alive() {
        // No death screen to click: request respawn immediately, once, and
        // wait for the server's `respawn` packet before doing anything else
        // (no physics, no movement packets while dead).
        if !state.respawn_pending {
            let command = PacketClientCommand {
                action_id: RESPAWN_ACTION_ID,
            };
            let mut w = PacketWriter::new();
            command.encode(&mut w)?;
            conn.send_packet(SERVERBOUND_CLIENT_COMMAND_ID, &w.freeze())
                .await?;
            state.respawn_pending = true;
            debug!(tick = state.clock.current(), "requested respawn");
        }
        return Ok(());
    }

    if !state.player.loaded
        && state.received_position
        && state.world.as_ref().is_some_and(|world| {
            let x = (state.player.position.x.floor() as i32).div_euclid(16);
            let z = (state.player.position.z.floor() as i32).div_euclid(16);
            world.has_chunk(x, z)
        })
    {
        conn.send_packet(SERVERBOUND_PLAYER_LOADED_ID, &[]).await?;
        state.player.mark_loaded();
        debug!(tick = state.clock.current(), "sent player_loaded");
        return Ok(());
    }

    if let Some(world) = state.world.as_ref() {
        // Resolve the active goal/overlay into this tick's input and facing,
        // then step vanilla physics on it.
        state.controller.drive(&mut state.player);
        state.player.tick_physics(world)?;
    }

    let Some(packet) = state.player.movement_packet() else {
        return Ok(());
    };
    let (id, payload) = match packet {
        MovementPacket::PositionLook(packet) => {
            let mut w = PacketWriter::new();
            packet.encode(&mut w)?;
            (SERVERBOUND_POSITION_LOOK_ID, w.freeze())
        }
        MovementPacket::Position(packet) => {
            let mut w = PacketWriter::new();
            packet.encode(&mut w)?;
            (SERVERBOUND_POSITION_ID, w.freeze())
        }
        MovementPacket::Look(packet) => {
            let mut w = PacketWriter::new();
            packet.encode(&mut w)?;
            (SERVERBOUND_LOOK_ID, w.freeze())
        }
        MovementPacket::StatusOnly(packet) => {
            let mut w = PacketWriter::new();
            packet.encode(&mut w)?;
            (SERVERBOUND_FLYING_ID, w.freeze())
        }
    };
    conn.send_packet(id, &payload).await?;
    Ok(())
}

/// Handles one clientbound play packet: sends any required response and
/// updates [`PlayState`].
async fn handle_clientbound(
    conn: &mut Connection,
    state: &mut PlayState,
    configuration: &ConfigurationData,
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
            state.received_position = true;
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
        CLIENTBOUND_MAP_CHUNK_ID => {
            let mut r = PacketReader::new(&packet.payload);
            let chunk = PacketMapChunk::decode(&mut r)?;
            let world = state.world.as_mut().ok_or_else(|| {
                MineRiderError::Protocol(
                    "map_chunk arrived before play login dimension".to_string(),
                )
            })?;
            world.insert_chunk(&chunk)?;
        }
        CLIENTBOUND_UNLOAD_CHUNK_ID => {
            let mut r = PacketReader::new(&packet.payload);
            let unload = PacketUnloadChunk::decode(&mut r)?;
            if let Some(world) = state.world.as_mut() {
                world.unload_chunk(&unload);
            }
        }
        CLIENTBOUND_BLOCK_CHANGE_ID => {
            let mut r = PacketReader::new(&packet.payload);
            let update = PacketBlockChange::decode(&mut r)?;
            if let Some(world) = state.world.as_mut() {
                world.apply_block_change(&update)?;
            }
        }
        CLIENTBOUND_ADD_RESOURCE_PACK_ID => {
            let mut r = PacketReader::new(&packet.payload);
            let pack = PacketCommonAddResourcePack::decode(&mut r)?;
            debug!(uuid = %pack.uuid, forced = pack.forced, "declining offered resource pack");
            let response = PacketResourcePackReceive {
                uuid: pack.uuid,
                result: RESOURCE_PACK_STATUS_DECLINED,
            };
            let mut w = PacketWriter::new();
            response.encode(&mut w)?;
            conn.send_packet(SERVERBOUND_RESOURCE_PACK_RECEIVE_ID, &w.freeze())
                .await?;
        }
        CLIENTBOUND_REMOVE_RESOURCE_PACK_ID => {
            // No client-side pack state to remove and no wire response.
        }
        CLIENTBOUND_MULTI_BLOCK_CHANGE_ID => {
            let mut r = PacketReader::new(&packet.payload);
            let update = PacketMultiBlockChange::decode(&mut r)?;
            if let Some(world) = state.world.as_mut() {
                world.apply_multi_block_change(&update)?;
            }
        }
        CLIENTBOUND_KICK_DISCONNECT_ID => {
            let Some(update) = decode_update(packet.id, &packet.payload)? else {
                return Err(MineRiderError::Protocol(
                    "kick_disconnect was not decoded as presentation state".to_string(),
                ));
            };
            state.apply_presentation(update);
            let reason = state
                .presentation
                .disconnect_reason
                .as_ref()
                .map(|reason| reason.plain_text())
                .unwrap_or_default();
            return Err(MineRiderError::Disconnected(reason));
        }
        other => {
            if apply_state_packet(state, configuration, other, &packet.payload)? {
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
fn apply_state_packet(
    state: &mut PlayState,
    configuration: &ConfigurationData,
    id: i32,
    payload: &[u8],
) -> Result<bool> {
    if let Some(update) = decode_update(id, payload)? {
        state.apply_presentation(update);
        return Ok(true);
    }
    let mut r = PacketReader::new(payload);
    match id {
        CLIENTBOUND_LOGIN_ID => {
            let p = PacketLogin::decode(&mut r)?;
            state.player.on_login(&p);
            let dimension = configuration
                .dimension_types
                .get(p.world_state.dimension as usize)
                .ok_or_else(|| {
                    MineRiderError::Protocol(format!(
                        "play login dimension index {} absent from {} dimension types",
                        p.world_state.dimension,
                        configuration.dimension_types.len()
                    ))
                })?;
            state.dimension = Some(dimension.clone());
            state.world = Some(World::new(dimension.clone()));
            state.emit(BotEvent::Login {
                entity_id: p.entity_id,
            });
            debug!(
                entity_id = p.entity_id,
                "play login: own entity id recorded"
            );
        }
        CLIENTBOUND_RESPAWN_ID => {
            let p = PacketRespawn::decode(&mut r)?;
            state.respawn_pending = false;
            state.player.on_respawn();
            state.received_position = false;
            let dimension = configuration
                .dimension_types
                .get(p.world_state.dimension as usize)
                .ok_or_else(|| {
                    MineRiderError::Protocol(format!(
                        "respawn dimension index {} absent from {} dimension types",
                        p.world_state.dimension,
                        configuration.dimension_types.len()
                    ))
                })?;
            state.dimension = Some(dimension.clone());
            state.world = Some(World::new(dimension.clone()));
            debug!("respawned; awaiting new position and chunk");
        }
        CLIENTBOUND_UPDATE_HEALTH_ID => {
            let p = PacketUpdateHealth::decode(&mut r)?;
            state.player.on_health(&p);
            state.emit(BotEvent::Health {
                health: p.health,
                food: p.food,
                saturation: p.food_saturation,
            });
            // Fire Death exactly once on the alive->dead edge.
            let alive = state.player.is_alive();
            if state.was_alive && !alive {
                state.emit(BotEvent::Death);
            }
            state.was_alive = alive;
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
            // Vanilla routes a velocity update addressed to the client's own
            // entity id into local physics (knockback, explosions, elytra
            // boost); every other id is a remote entity we merely track.
            if state.player.entity_id == Some(p.entity_id) {
                const VELOCITY_UNIT: f64 = 8000.0;
                state.player.apply_velocity(Vec3 {
                    x: p.velocity.x as f64 / VELOCITY_UNIT,
                    y: p.velocity.y as f64 / VELOCITY_UNIT,
                    z: p.velocity.z as f64 / VELOCITY_UNIT,
                });
            } else {
                state.entities.velocity(&p);
            }
        }
        CLIENTBOUND_ENTITY_HEAD_ROTATION_ID => {
            let p = PacketEntityHeadRotation::decode(&mut r)?;
            state.entities.head_rotation(&p);
        }
        CLIENTBOUND_OPEN_WINDOW_ID => {
            let p = PacketOpenWindow::decode(&mut r)?;
            debug!(
                window_id = p.window_id,
                kind = p.inventory_type,
                "opened container"
            );
            state.inventory.open_window(&p);
        }
        CLIENTBOUND_CLOSE_WINDOW_ID => {
            let p = PacketCloseWindow::decode(&mut r)?;
            debug!(window_id = p.window_id, "closed container");
            state.inventory.close_window(&p);
        }
        CLIENTBOUND_WINDOW_ITEMS_ID => {
            let p = PacketWindowItems::decode(&mut r)?;
            state.inventory.window_items(&p);
        }
        CLIENTBOUND_SET_SLOT_ID => {
            let p = PacketSetSlot::decode(&mut r)?;
            state.inventory.set_slot(&p);
        }
        CLIENTBOUND_SET_CURSOR_ITEM_ID => {
            let p = PacketSetCursorItem::decode(&mut r)?;
            state.inventory.set_cursor_item(&p);
        }
        CLIENTBOUND_CRAFT_PROGRESS_BAR_ID => {
            let p = PacketCraftProgressBar::decode(&mut r)?;
            state.inventory.craft_progress_bar(&p);
        }
        CLIENTBOUND_HELD_ITEM_SLOT_ID => {
            let p = PacketHeldItemSlot::decode(&mut r)?;
            state.inventory.held_item_slot(&p);
        }
        CLIENTBOUND_PLAYER_INFO_ID => {
            let p = PacketPlayerInfo::decode(&mut r)?;
            let changes = state.players.apply_info(&p);
            for (uuid, name) in changes.joined {
                state.emit(BotEvent::PlayerJoined { uuid, name });
            }
        }
        CLIENTBOUND_PLAYER_REMOVE_ID => {
            let p = PacketPlayerRemove::decode(&mut r)?;
            for uuid in state.players.apply_remove(&p) {
                state.emit(BotEvent::PlayerLeft { uuid });
            }
        }
        CLIENTBOUND_UPDATE_TIME_ID => {
            let p = PacketUpdateTime::decode(&mut r)?;
            // Vanilla stores `time` as the day time; a negative value means
            // the day-night cycle is frozen (its magnitude is still the
            // time), so normalise into 0..24000.
            let time_of_day = p.time.rem_euclid(24_000);
            state.world_time = time_of_day;
            state.emit(BotEvent::Time { time_of_day });
        }
        CLIENTBOUND_GAME_STATE_CHANGE_ID => {
            let p = PacketGameStateChange::decode(&mut r)?;
            match p.reason {
                GAME_STATE_BEGIN_RAINING => {
                    state.raining = true;
                    state.emit(BotEvent::Weather { raining: true });
                }
                GAME_STATE_END_RAINING => {
                    state.raining = false;
                    state.emit(BotEvent::Weather { raining: false });
                }
                GAME_STATE_CHANGE_GAMEMODE => {
                    debug!(gamemode = p.game_mode, "gamemode changed");
                }
                _ => {}
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_is_an_independent_copy_of_live_state() {
        let (event_tx, _event_rx) =
            broadcast::channel(crate::minecraft::event::EVENT_CHANNEL_CAPACITY);
        let mut state = PlayState::new(event_tx);
        state.player.health = 7.5;
        state.presentation.action_bar =
            Some(crate::minecraft::text::TextComponent::literal("before"));
        let snap = state.snapshot(42);

        assert_eq!(snap.tick, 42);
        assert_eq!(snap.player.health, 7.5);

        // Mutating the source afterward must not affect an already-taken
        // snapshot: it's a real clone, not a shared reference.
        state.player.health = 20.0;
        state.presentation.action_bar =
            Some(crate::minecraft::text::TextComponent::literal("after"));
        assert_eq!(
            snap.player.health, 7.5,
            "snapshot is independent of live state"
        );
        assert_eq!(
            snap.presentation
                .action_bar
                .as_ref()
                .expect("snapshot action bar")
                .plain_text(),
            "before"
        );
    }
}
