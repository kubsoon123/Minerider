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
    PacketArmAnimation, PacketBlockChange, PacketChatCommand, PacketChatMessage,
    PacketChunkBatchFinished, PacketChunkBatchReceived, PacketClientCommand, PacketCloseWindow,
    PacketCraftProgressBar, PacketEntityDestroy, PacketEntityHeadRotation, PacketEntityLook,
    PacketEntityMoveLook, PacketEntityTeleport, PacketEntityVelocity, PacketExperience,
    PacketGameStateChange, PacketHeldItemSlot, PacketHeldItemSlotServerbound, PacketKeepAlive,
    PacketLogin, PacketMapChunk, PacketMultiBlockChange, PacketOpenWindow, PacketPing,
    PacketPlayerInfo, PacketPlayerInput, PacketPlayerInputInputs, PacketPlayerRemove, PacketPong,
    PacketPosition, PacketRelEntityMove, PacketResourcePackReceive, PacketRespawn,
    PacketSetCursorItem, PacketSetPlayerInventory, PacketSetSlot, PacketSpawnEntity,
    PacketSyncEntityPosition, PacketTeleportConfirm, PacketTileEntityData, PacketUnloadChunk,
    PacketUpdateHealth, PacketUpdateLight, PacketUpdateTime, PacketUseItem, PacketWindowItems,
    CLIENTBOUND_ADD_RESOURCE_PACK_ID, CLIENTBOUND_BLOCK_CHANGE_ID,
    CLIENTBOUND_CHUNK_BATCH_FINISHED_ID, CLIENTBOUND_CHUNK_BATCH_START_ID,
    CLIENTBOUND_CLOSE_WINDOW_ID, CLIENTBOUND_CRAFT_PROGRESS_BAR_ID, CLIENTBOUND_ENTITY_DESTROY_ID,
    CLIENTBOUND_ENTITY_HEAD_ROTATION_ID, CLIENTBOUND_ENTITY_LOOK_ID,
    CLIENTBOUND_ENTITY_MOVE_LOOK_ID, CLIENTBOUND_ENTITY_TELEPORT_ID,
    CLIENTBOUND_ENTITY_VELOCITY_ID, CLIENTBOUND_EXPERIENCE_ID, CLIENTBOUND_GAME_STATE_CHANGE_ID,
    CLIENTBOUND_HELD_ITEM_SLOT_ID, CLIENTBOUND_KEEP_ALIVE_ID, CLIENTBOUND_KICK_DISCONNECT_ID,
    CLIENTBOUND_LOGIN_ID, CLIENTBOUND_MAP_CHUNK_ID, CLIENTBOUND_MULTI_BLOCK_CHANGE_ID,
    CLIENTBOUND_OPEN_WINDOW_ID, CLIENTBOUND_PING_ID, CLIENTBOUND_PLAYER_INFO_ID,
    CLIENTBOUND_PLAYER_REMOVE_ID, CLIENTBOUND_POSITION_ID, CLIENTBOUND_REL_ENTITY_MOVE_ID,
    CLIENTBOUND_REMOVE_RESOURCE_PACK_ID, CLIENTBOUND_RESPAWN_ID, CLIENTBOUND_SET_CURSOR_ITEM_ID,
    CLIENTBOUND_SET_PLAYER_INVENTORY_ID, CLIENTBOUND_SET_SLOT_ID, CLIENTBOUND_SPAWN_ENTITY_ID,
    CLIENTBOUND_SYNC_ENTITY_POSITION_ID, CLIENTBOUND_TILE_ENTITY_DATA_ID,
    CLIENTBOUND_UNLOAD_CHUNK_ID, CLIENTBOUND_UPDATE_HEALTH_ID, CLIENTBOUND_UPDATE_LIGHT_ID,
    CLIENTBOUND_UPDATE_TIME_ID, CLIENTBOUND_WINDOW_ITEMS_ID, SERVERBOUND_ARM_ANIMATION_ID,
    SERVERBOUND_CHAT_COMMAND_ID, SERVERBOUND_CHAT_MESSAGE_ID, SERVERBOUND_CHUNK_BATCH_RECEIVED_ID,
    SERVERBOUND_CLIENT_COMMAND_ID, SERVERBOUND_FLYING_ID, SERVERBOUND_HELD_ITEM_SLOT_ID,
    SERVERBOUND_KEEP_ALIVE_ID, SERVERBOUND_LOOK_ID, SERVERBOUND_PLAYER_INPUT_ID,
    SERVERBOUND_PLAYER_LOADED_ID, SERVERBOUND_PONG_ID, SERVERBOUND_POSITION_ID,
    SERVERBOUND_POSITION_LOOK_ID, SERVERBOUND_RESOURCE_PACK_RECEIVE_ID,
    SERVERBOUND_TELEPORT_CONFIRM_ID, SERVERBOUND_TICK_END_ID, SERVERBOUND_USE_ITEM_ID,
    SERVERBOUND_WINDOW_CLICK_ID,
};
use minerider_protocol::generated::v1_21_4::types::{PacketCommonAddResourcePack, Vec2f};
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
use crate::minecraft::control::{BotCommand, Controller, Hand};
use crate::minecraft::coverage::{clientbound_coverage, CoverageClass};
use crate::minecraft::entity::EntityStore;
use crate::minecraft::event::BotEvent;
use crate::minecraft::hud::{decode_update as decode_hud_update, GameMode, HudEvent, HudState};
use crate::minecraft::inventory::{InventoryClickRequest, InventoryEvent, InventoryState};
use crate::minecraft::physics::Vec3;
use crate::minecraft::player::{LocalPlayer, MovementPacket};
use crate::minecraft::players::PlayerList;
use crate::minecraft::presentation::{
    decode_update, ChatKind, PresentationEvent, PresentationState,
};
use crate::minecraft::scoreboard::{decode_update as decode_scoreboard_update, ScoreboardState};
use crate::minecraft::shared_world::SharedWorldContext;
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
    /// Objectives, display slots, scores, teams, and team membership.
    pub scoreboard: ScoreboardState,
    /// Local-player HUD state and world context omitted from the physics core.
    pub hud: HudState,
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
    world_sharing: Option<SharedWorldContext>,
    /// Whether a `client_command` respawn request is outstanding: set when we
    /// detect death, cleared when the `respawn` packet arrives. Prevents
    /// resending the request every tick while the server processes it.
    respawn_pending: bool,
    /// Whether the bot was alive on the previous vitals update, so a death
    /// edge fires the `Death` event exactly once.
    was_alive: bool,
    /// Monotonic per-session counter for `use_item`'s `sequence` field.
    /// Starts fresh (`0`) every time a new [`PlayState`] is built — i.e.
    /// every new connection/reconnect — since a sequence number only needs
    /// to be unique *within* one session's acknowledgement stream, never
    /// across sessions.
    next_action_sequence: i32,
    /// The seven-flag `player_input` bitset last sent to the server. Vanilla
    /// sends `player_input` only when the input *changes*, so this tracks the
    /// last value to suppress redundant per-tick resends. Starts at `0` (no
    /// keys) every session, matching the server's assumption for a freshly
    /// joined player, so an idle bot sends nothing.
    last_player_input: u8,
    /// Vanilla's adaptive chunk-batch pacing estimate; drives the
    /// `chunks_per_tick` sent in `chunk_batch_received`.
    chunk_batch: crate::minecraft::chunk_batch::ChunkBatchSizeCalculator,
    /// When the current chunk batch started processing (`chunk_batch_start`),
    /// used to time it at `chunk_batch_finished`. `None` outside a batch.
    chunk_batch_started_at: Option<std::time::Instant>,
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
        Self::with_world_sharing(event_tx, None)
    }

    fn with_world_sharing(
        event_tx: broadcast::Sender<BotEvent>,
        world_sharing: Option<SharedWorldContext>,
    ) -> Self {
        Self {
            player: LocalPlayer::default(),
            entities: EntityStore::default(),
            inventory: InventoryState::default(),
            players: PlayerList::default(),
            presentation: PresentationState::default(),
            scoreboard: ScoreboardState::default(),
            hud: HudState::default(),
            world_time: 0,
            raining: false,
            controller: Controller::default(),
            clock: TickClock::default(),
            event_tx,
            received_position: false,
            dimension: None,
            world: None,
            world_sharing,
            respawn_pending: false,
            was_alive: true,
            next_action_sequence: 0,
            last_player_input: 0,
            chunk_batch: crate::minecraft::chunk_batch::ChunkBatchSizeCalculator::default(),
            chunk_batch_started_at: None,
        }
    }

    /// The next `use_item` sequence number for this session, never repeated
    /// within it.
    fn next_action_sequence(&mut self) -> i32 {
        self.next_action_sequence = self.next_action_sequence.wrapping_add(1);
        self.next_action_sequence
    }

    /// Broadcasts an event to all subscribers; a full/closed channel is
    /// ignored (observers must never back-pressure the play loop).
    fn emit(&self, event: BotEvent) {
        let _ = self.event_tx.send(event);
    }

    fn apply_presentation(&mut self, update: crate::minecraft::presentation::PresentationUpdate) {
        let event = self.presentation.apply(update);
        self.emit(BotEvent::Presentation(Box::new(event.clone())));
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

    fn apply_scoreboard(&mut self, update: crate::minecraft::scoreboard::ScoreboardUpdate) {
        let event = self.scoreboard.apply(update);
        self.emit(BotEvent::Scoreboard(Box::new(event)));
    }

    fn emit_hud(&self, event: HudEvent) {
        self.emit(BotEvent::Hud(Box::new(event)));
    }

    fn apply_hud(&mut self, update: crate::minecraft::hud::HudUpdate) {
        if let Some(event) = self.hud.apply(update) {
            self.emit_hud(event);
        }
    }

    fn emit_inventory(&self, event: InventoryEvent) {
        self.emit(BotEvent::Inventory(Box::new(event)));
    }

    fn emit_inventory_events(&self, events: impl IntoIterator<Item = InventoryEvent>) {
        for event in events {
            self.emit_inventory(event);
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
            scoreboard: self.scoreboard.clone(),
            hud: self.hud.clone(),
            world_time: self.world_time,
            raining: self.raining,
            dimension: self.dimension.clone(),
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
    pub scoreboard: ScoreboardState,
    pub hud: HudState,
    pub world_time: i64,
    pub raining: bool,
    /// Retained dimension properties (min/max Y, coordinate scale, ...) for
    /// whichever dimension the player is currently in — cheap to clone every
    /// tick (a handful of scalar fields), unlike the full `World`/chunk data
    /// this snapshot deliberately still excludes (see [`PlayState::snapshot`]).
    /// `None` before the first `login`/`respawn` packet.
    pub dimension: Option<DimensionType>,
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
    world_sharing: Option<SharedWorldContext>,
) -> Result<()> {
    let mut state = PlayState::with_world_sharing(event_tx, world_sharing);
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
                    // Outbound chat actions go straight to their distinct
                    // protocol packets rather than into movement state.
                    Some(action @ (BotCommand::Chat(_) | BotCommand::Command(_))) => {
                        send_outbound_chat_action(conn, &action).await?
                    }
                    Some(BotCommand::InventoryClick(request)) => {
                        send_inventory_click(conn, &mut state, request).await?
                    }
                    Some(BotCommand::UseItem(hand)) => send_use_item(conn, &mut state, hand).await?,
                    Some(BotCommand::Swing(hand)) => send_swing(conn, hand).await?,
                    Some(BotCommand::SelectHotbarSlot(slot)) => {
                        send_select_hotbar_slot(conn, &mut state, slot).await?
                    }
                    Some(command) => apply_command(&mut state, command),
                    None => control_open = false,
                }
                let _ = state_tx.send(state.snapshot(state.clock.current()));
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

/// The message is sent unsigned (no cryptographic signature). Offline-mode
/// servers and servers with `enforce-secure-profile=false` accept this;
/// servers that enforce secure chat will reject or kick unsigned messages —
/// full message signing (a per-message ECDSA signature over the chat session
/// key from `/player/certificates`) is a deliberate follow-up.
async fn send_outbound_chat_action(conn: &mut Connection, action: &BotCommand) -> Result<()> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let (id, payload) = encode_outbound_chat_action(action, timestamp, rand::random())?;
    conn.send_packet(id, &payload).await?;
    match action {
        BotCommand::Chat(message) => debug!(%message, "sent chat"),
        BotCommand::Command(command) => debug!(%command, "sent command"),
        _ => unreachable!("encoder only accepts outbound chat actions"),
    }
    Ok(())
}

/// Encodes exactly the packet selected by the typed action. Content never
/// changes packet kind (in particular, there is no leading-slash inference).
fn encode_outbound_chat_action(
    action: &BotCommand,
    timestamp: i64,
    salt: i64,
) -> Result<(i32, Vec<u8>)> {
    action
        .validate()
        .map_err(|error| MineRiderError::Protocol(error.to_string()))?;
    let mut output = PacketWriter::new();
    let id = match action {
        BotCommand::Chat(message) => {
            PacketChatMessage {
                message: message.clone(),
                timestamp,
                salt,
                signature: None,
                // No message-chain acknowledgement is tracked, so
                // acknowledge zero prior messages: offset 0 and an empty
                // (all-zero) 3-byte bitset.
                offset: 0,
                acknowledged: vec![0u8; 3],
            }
            .encode(&mut output)?;
            SERVERBOUND_CHAT_MESSAGE_ID
        }
        BotCommand::Command(command) => {
            PacketChatCommand {
                command: command.clone(),
            }
            .encode(&mut output)?;
            SERVERBOUND_CHAT_COMMAND_ID
        }
        _ => {
            return Err(MineRiderError::Protocol(
                "attempted to encode a non-chat control action as chat".into(),
            ));
        }
    };
    Ok((id, output.into_inner().to_vec()))
}

async fn send_inventory_click(
    conn: &mut Connection,
    state: &mut PlayState,
    request: InventoryClickRequest,
) -> Result<()> {
    let transaction_id = request.transaction_id;
    let creative = state.hud.game_mode == GameMode::Creative;
    let prepared = match state
        .inventory
        .prepare_click(request, state.clock.current(), creative)
    {
        Ok(prepared) => prepared,
        Err(error) => {
            let event = state.inventory.reject_transaction(transaction_id, error);
            state.emit_inventory(event);
            return Ok(());
        }
    };
    state.emit_inventory(prepared.event);
    let mut output = PacketWriter::new();
    prepared.packet.encode(&mut output)?;
    conn.send_packet(SERVERBOUND_WINDOW_CLICK_ID, &output.freeze())
        .await?;
    let events = state.inventory.mark_sent(transaction_id);
    state.emit_inventory_events(events);
    Ok(())
}

/// Sends protocol-769 `use_item` (right-click activation of the held item).
/// `sequence` is this session's next value (see [`PlayState::next_action_sequence`]
/// — never repeated within one session, and implicitly reset on every new
/// session since [`PlayState`] itself is rebuilt on connect/reconnect); the
/// camera-angle fields introduced in 1.21.2 carry the player's current
/// yaw/pitch at send time. There is no dedicated clientbound acknowledgement
/// for this specific packet, so returning `Ok(())` here means only "the
/// packet was sent", not "the server accepted the interaction" — the same
/// contract [`crate::core::supervisor::SupervisorHandle::use_item`] documents.
async fn send_use_item(conn: &mut Connection, state: &mut PlayState, hand: Hand) -> Result<()> {
    let packet = PacketUseItem {
        hand: hand.wire_value(),
        sequence: state.next_action_sequence(),
        rotation: Vec2f {
            x: state.player.position.yaw,
            y: state.player.position.pitch,
        },
    };
    let mut output = PacketWriter::new();
    packet.encode(&mut output)?;
    conn.send_packet(SERVERBOUND_USE_ITEM_ID, &output.freeze())
        .await
}

/// Sends protocol-769 `arm_animation` (the swing animation), independent of
/// [`send_use_item`] — vanilla sends these as separate packets and so does
/// this API; see [`crate::core::supervisor::SupervisorHandle::swing`].
async fn send_swing(conn: &mut Connection, hand: Hand) -> Result<()> {
    let packet = PacketArmAnimation {
        hand: hand.wire_value(),
    };
    let mut output = PacketWriter::new();
    packet.encode(&mut output)?;
    conn.send_packet(SERVERBOUND_ARM_ANIMATION_ID, &output.freeze())
        .await
}

/// Sends `held_item_slot` to select the active hotbar slot (`0..=8`) and
/// updates the local selection so a following `use_item`/`swing` acts on the
/// newly held item — mirroring the vanilla client, which tracks its own
/// selected slot rather than waiting for a server echo. The `0..=8` range is
/// already enforced by `BotCommand::validate`, but the inventory setter
/// re-checks it (it is the authority on the tracked selection).
async fn send_select_hotbar_slot(
    conn: &mut Connection,
    state: &mut PlayState,
    slot: i16,
) -> Result<()> {
    let packet = PacketHeldItemSlotServerbound { slot_id: slot };
    let mut output = PacketWriter::new();
    packet.encode(&mut output)?;
    conn.send_packet(SERVERBOUND_HELD_ITEM_SLOT_ID, &output.freeze())
        .await?;
    let event = state.inventory.select_hotbar_slot(slot);
    state.emit_inventory(event);
    debug!(slot, "selected hotbar slot");
    Ok(())
}

/// Runs vanilla's per-tick play-entry and movement behavior.
async fn handle_tick(conn: &mut Connection, state: &mut PlayState) -> Result<()> {
    let expired = state.inventory.expire_transactions(state.clock.current());
    state.emit_inventory_events(expired);
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
        state.emit(BotEvent::Spawned);
        debug!(tick = state.clock.current(), "sent player_loaded");
        return Ok(());
    }

    if let Some(world) = state.world.as_ref() {
        // Resolve the active goal/overlay into this tick's input and facing,
        // then step vanilla physics on it.
        state.controller.drive(&mut state.player);
        state.player.tick_physics(world)?;
    }

    // Vanilla's `LocalPlayer.aiStep` sends `player_input` (the seven movement
    // keys) whenever the pressed set changes, before `sendPosition`. `drive`
    // above has finalized this tick's input; send it (on change) before the
    // movement packet.
    send_player_input_if_changed(conn, state).await?;

    if let Some(packet) = state.player.movement_packet() {
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
    }

    // Vanilla sends `tick_end` (a fieldless marker) at the very end of every
    // client tick once the player is in a ticking world — after movement,
    // once per tick. The `loaded` gate keeps a mid-join client (not yet
    // ticking in-world) from sending it, matching vanilla, and keeps it out
    // of pre-spawn packet expectations.
    if state.player.loaded {
        conn.send_packet(SERVERBOUND_TICK_END_ID, &[]).await?;
    }
    Ok(())
}

/// The seven-flag `player_input` bitset for the player's current movement
/// keys, matching `ServerboundPlayerInputPacket`'s flag layout. Forward and
/// left are the positive impulse directions (vanilla `leftImpulse` is
/// positive when strafing left).
fn player_input_flags(input: &crate::minecraft::player::MovementInput) -> u8 {
    let mut flags = 0u8;
    if input.forward > 0.0 {
        flags |= PacketPlayerInputInputs::FORWARD;
    } else if input.forward < 0.0 {
        flags |= PacketPlayerInputInputs::BACKWARD;
    }
    if input.strafe > 0.0 {
        flags |= PacketPlayerInputInputs::LEFT;
    } else if input.strafe < 0.0 {
        flags |= PacketPlayerInputInputs::RIGHT;
    }
    if input.jump {
        flags |= PacketPlayerInputInputs::JUMP;
    }
    if input.sneak {
        flags |= PacketPlayerInputInputs::SHIFT;
    }
    if input.sprint {
        flags |= PacketPlayerInputInputs::SPRINT;
    }
    flags
}

/// Sends `player_input` only when this tick's movement-key set differs from
/// the last one sent, exactly like vanilla. Gated on `loaded` by its only
/// caller (the active-tick path), so a mid-join client sends nothing.
async fn send_player_input_if_changed(conn: &mut Connection, state: &mut PlayState) -> Result<()> {
    if !state.player.loaded {
        return Ok(());
    }
    let flags = player_input_flags(&state.player.input);
    if flags == state.last_player_input {
        return Ok(());
    }
    state.last_player_input = flags;
    let packet = PacketPlayerInput {
        inputs: PacketPlayerInputInputs(flags),
    };
    let mut w = PacketWriter::new();
    packet.encode(&mut w)?;
    conn.send_packet(SERVERBOUND_PLAYER_INPUT_ID, &w.freeze())
        .await?;
    debug!(flags, tick = state.clock.current(), "sent player_input");
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
        CLIENTBOUND_PING_ID => {
            // Play-state ping/pong: echo the id straight back, exactly like
            // the vanilla client. Distinct from the status-state ping and
            // from keep-alive; a server that pings and gets no pong within
            // its window disconnects the client.
            let mut r = PacketReader::new(&packet.payload);
            let ping = PacketPing::decode(&mut r)?;
            let pong = PacketPong { id: ping.id };
            let mut w = PacketWriter::new();
            pong.encode(&mut w)?;
            conn.send_packet(SERVERBOUND_PONG_ID, &w.freeze()).await?;
            debug!(id = ping.id, "replied pong to play ping");
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
            // Vanilla's `handleMovePlayer` sends a full position+rotation
            // packet immediately after the teleport confirm — for every
            // teleport, including this initial spawn one before the world is
            // loaded — so the server hears the client's acknowledged
            // absolute position, not just the numeric confirm. Resyncs the
            // movement baseline so the next tick doesn't resend it.
            let ack_move = state.player.teleport_ack_movement();
            let mut w = PacketWriter::new();
            ack_move.encode(&mut w)?;
            conn.send_packet(SERVERBOUND_POSITION_LOOK_ID, &w.freeze())
                .await?;
            debug!(
                x = state.player.position.x,
                y = state.player.position.y,
                z = state.player.position.z,
                teleport_id = sync.teleport_id,
                "confirmed teleport and sent position_look"
            );
        }
        CLIENTBOUND_CHUNK_BATCH_START_ID => {
            // Marks the start of a chunk batch: vanilla times how long the
            // batch takes to process to pace the next one (see
            // `chunk_batch::ChunkBatchSizeCalculator`).
            state.chunk_batch_started_at = Some(std::time::Instant::now());
        }
        CLIENTBOUND_CHUNK_BATCH_FINISHED_ID => {
            let mut r = PacketReader::new(&packet.payload);
            let finished = PacketChunkBatchFinished::decode(&mut r)?;
            // Vanilla's chunk-batch pacing: report an adaptive
            // `chunks_per_tick` (7ms budget / measured nanos-per-chunk),
            // clamped and rolling-averaged, so a fast client is fed faster
            // and a slow one throttled — not a naive echo of the batch size.
            // Fall back to a zero-length elapsed if we never saw the matching
            // start (the estimate then just carries its current value).
            let elapsed = state
                .chunk_batch_started_at
                .take()
                .map(|start| start.elapsed())
                .unwrap_or_default();
            state
                .chunk_batch
                .record_batch(elapsed, finished.batch_size.max(0) as u32);
            let chunks_per_tick = state.chunk_batch.desired_chunks_per_tick();
            let ack = PacketChunkBatchReceived { chunks_per_tick };
            let mut w = PacketWriter::new();
            ack.encode(&mut w)?;
            conn.send_packet(SERVERBOUND_CHUNK_BATCH_RECEIVED_ID, &w.freeze())
                .await?;
            debug!(
                batch_size = finished.batch_size,
                chunks_per_tick, "acknowledged chunk batch"
            );
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
        CLIENTBOUND_TILE_ENTITY_DATA_ID => {
            let mut r = PacketReader::new(&packet.payload);
            let update = PacketTileEntityData::decode(&mut r)?;
            if let Some(world) = state.world.as_mut() {
                world.apply_block_entity_update(&update)?;
            }
        }
        CLIENTBOUND_UPDATE_LIGHT_ID => {
            let mut r = PacketReader::new(&packet.payload);
            let update = PacketUpdateLight::decode(&mut r)?;
            if let Some(world) = state.world.as_mut() {
                world.apply_light_update(&update)?;
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
    if let Some(update) = decode_scoreboard_update(id, payload)? {
        state.apply_scoreboard(update);
        return Ok(true);
    }
    if let Some(update) = decode_hud_update(id, payload)? {
        state.apply_hud(update);
        return Ok(true);
    }
    let mut r = PacketReader::new(payload);
    match id {
        CLIENTBOUND_LOGIN_ID => {
            let p = PacketLogin::decode(&mut r)?;
            state.player.on_login(&p);
            state.hud.on_login(&p);
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
            state.world = Some(match &state.world_sharing {
                Some(sharing) => sharing.world(
                    dimension.clone(),
                    p.world_state.name.clone(),
                    p.world_state.hashed_seed,
                ),
                None => World::new(dimension.clone()),
            });
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
            let hud_event = state.hud.on_respawn(&p);
            state.emit_hud(hud_event);
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
            state.world = Some(match &state.world_sharing {
                Some(sharing) => sharing.world(
                    dimension.clone(),
                    p.world_state.name.clone(),
                    p.world_state.hashed_seed,
                ),
                None => World::new(dimension.clone()),
            });
            debug!("respawned; awaiting new position and chunk");
        }
        CLIENTBOUND_UPDATE_HEALTH_ID => {
            let p = PacketUpdateHealth::decode(&mut r)?;
            state.player.on_health(&p);
            let hud_event = state.hud.on_health(&p);
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
            state.emit_hud(hud_event);
        }
        CLIENTBOUND_EXPERIENCE_ID => {
            let p = PacketExperience::decode(&mut r)?;
            state.player.on_experience(&p);
            let hud_event = state
                .hud
                .on_experience(p.experience_bar, p.level, p.total_experience);
            state.emit_hud(hud_event);
        }
        CLIENTBOUND_SPAWN_ENTITY_ID => {
            let p = PacketSpawnEntity::decode(&mut r)?;
            state.entities.spawn(&p);
            state.emit(BotEvent::EntitySpawned {
                entity_id: p.entity_id,
                uuid: p.object_uuid,
                kind: p.r#type,
            });
        }
        CLIENTBOUND_ENTITY_DESTROY_ID => {
            let p = PacketEntityDestroy::decode(&mut r)?;
            state.entities.destroy(&p.entity_ids);
            for entity_id in &p.entity_ids {
                state.emit(BotEvent::EntityRemoved {
                    entity_id: *entity_id,
                });
            }
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
            let events = state.inventory.open_window(&p);
            state.emit_inventory_events(events);
        }
        CLIENTBOUND_CLOSE_WINDOW_ID => {
            let p = PacketCloseWindow::decode(&mut r)?;
            debug!(window_id = p.window_id, "closed container");
            let events = state.inventory.close_window(&p);
            state.emit_inventory_events(events);
        }
        CLIENTBOUND_WINDOW_ITEMS_ID => {
            let p = PacketWindowItems::decode(&mut r)?;
            let events = state.inventory.window_items(&p);
            state.emit_inventory_events(events);
        }
        CLIENTBOUND_SET_SLOT_ID => {
            let p = PacketSetSlot::decode(&mut r)?;
            let events = state.inventory.set_slot(&p);
            state.emit_inventory_events(events);
        }
        CLIENTBOUND_SET_CURSOR_ITEM_ID => {
            let p = PacketSetCursorItem::decode(&mut r)?;
            let event = state.inventory.set_cursor_item(&p);
            state.emit_inventory(event);
        }
        CLIENTBOUND_CRAFT_PROGRESS_BAR_ID => {
            let p = PacketCraftProgressBar::decode(&mut r)?;
            if let Some(event) = state.inventory.craft_progress_bar(&p) {
                state.emit_inventory(event);
            }
        }
        CLIENTBOUND_HELD_ITEM_SLOT_ID => {
            let p = PacketHeldItemSlot::decode(&mut r)?;
            let inventory_event = state.inventory.held_item_slot(&p);
            state.emit_inventory(inventory_event);
            let hud_event = state.hud.on_selected_hotbar(&p);
            state.emit_hud(hud_event);
        }
        CLIENTBOUND_SET_PLAYER_INVENTORY_ID => {
            let p = PacketSetPlayerInventory::decode(&mut r)?;
            let inventory_events = state.inventory.set_player_inventory(&p);
            state.emit_inventory_events(inventory_events);
            let hud_event = state.hud.on_player_inventory(&p);
            state.emit_hud(hud_event);
        }
        CLIENTBOUND_PLAYER_INFO_ID => {
            let p = PacketPlayerInfo::decode(&mut r)?;
            let changes = state.players.apply_info(&p);
            for (uuid, name) in changes.joined {
                state.emit(BotEvent::PlayerJoined { uuid, name });
            }
            state.emit_hud(HudEvent::PlayerListChanged {
                updated: changes.updated,
                removed: Vec::new(),
                rejected: changes.rejected,
            });
        }
        CLIENTBOUND_PLAYER_REMOVE_ID => {
            let p = PacketPlayerRemove::decode(&mut r)?;
            let removed = state.players.apply_remove(&p);
            for &uuid in &removed {
                state.emit(BotEvent::PlayerLeft { uuid });
            }
            state.emit_hud(HudEvent::PlayerListChanged {
                updated: Vec::new(),
                removed,
                rejected: 0,
            });
        }
        CLIENTBOUND_UPDATE_TIME_ID => {
            let p = PacketUpdateTime::decode(&mut r)?;
            // Vanilla stores `time` as the day time; a negative value means
            // the day-night cycle is frozen (its magnitude is still the
            // time), so normalise into 0..24000.
            let time_of_day = p.time.rem_euclid(24_000);
            state.world_time = time_of_day;
            let hud_event = state.hud.on_time(&p);
            state.emit(BotEvent::Time { time_of_day });
            state.emit_hud(hud_event);
        }
        CLIENTBOUND_GAME_STATE_CHANGE_ID => {
            let p = PacketGameStateChange::decode(&mut r)?;
            let hud_event = state.hud.on_game_state(&p);
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
            if let Some(hud_event) = hud_event {
                state.emit_hud(hud_event);
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
        state.scoreboard.objectives.insert(
            "before".into(),
            crate::minecraft::scoreboard::Objective {
                name: "before".into(),
                display_name: crate::minecraft::text::TextComponent::literal("Before"),
                render_type: crate::minecraft::scoreboard::ObjectiveRenderType::Integer,
                number_format: None,
            },
        );
        state.hud.cooldowns.insert("before".into(), 4);
        let snap = state.snapshot(42);

        assert_eq!(snap.tick, 42);
        assert_eq!(snap.player.health, 7.5);

        // Mutating the source afterward must not affect an already-taken
        // snapshot: it's a real clone, not a shared reference.
        state.player.health = 20.0;
        state.presentation.action_bar =
            Some(crate::minecraft::text::TextComponent::literal("after"));
        state.scoreboard.objectives.clear();
        state.hud.cooldowns.clear();
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
        assert!(snap.scoreboard.objectives.contains_key("before"));
        assert_eq!(snap.hud.cooldowns.get("before"), Some(&4));
    }

    #[test]
    fn outbound_chat_and_commands_use_distinct_protocol_packets() {
        let (chat_id, chat_payload) =
            encode_outbound_chat_action(&BotCommand::Chat("hello".into()), 123, 456).unwrap();
        assert_eq!(chat_id, SERVERBOUND_CHAT_MESSAGE_ID);
        let chat = PacketChatMessage::decode(&mut PacketReader::new(&chat_payload)).unwrap();
        assert_eq!(chat.message, "hello");
        assert_eq!(chat.timestamp, 123);
        assert_eq!(chat.salt, 456);
        assert!(chat.signature.is_none());
        assert_eq!(chat.acknowledged, vec![0, 0, 0]);

        let (command_id, command_payload) =
            encode_outbound_chat_action(&BotCommand::Command("say hello".into()), 999, 999)
                .unwrap();
        assert_eq!(command_id, SERVERBOUND_CHAT_COMMAND_ID);
        let command = PacketChatCommand::decode(&mut PacketReader::new(&command_payload)).unwrap();
        assert_eq!(command.command, "say hello");
    }

    #[test]
    fn slash_content_is_rejected_instead_of_changing_packet_kind() {
        assert!(encode_outbound_chat_action(&BotCommand::Chat("/help".into()), 0, 0).is_err());
        assert!(encode_outbound_chat_action(&BotCommand::Command("/help".into()), 0, 0).is_err());
    }

    #[test]
    fn action_sequence_increments_and_never_repeats_within_a_session() {
        let (event_tx, _rx) = broadcast::channel(crate::minecraft::event::EVENT_CHANNEL_CAPACITY);
        let mut state = PlayState::new(event_tx);
        assert_eq!(state.next_action_sequence(), 1);
        assert_eq!(state.next_action_sequence(), 2);
        assert_eq!(state.next_action_sequence(), 3);
    }

    #[test]
    fn player_input_flags_map_each_movement_key_to_its_vanilla_bit() {
        use crate::minecraft::player::MovementInput;

        // Idle: no keys, no flags.
        assert_eq!(player_input_flags(&MovementInput::default()), 0);

        // Forward and backward are the two signs of the forward impulse and
        // are mutually exclusive.
        let fwd = MovementInput {
            forward: 1.0,
            ..Default::default()
        };
        assert_eq!(player_input_flags(&fwd), PacketPlayerInputInputs::FORWARD);
        let back = MovementInput {
            forward: -1.0,
            ..Default::default()
        };
        assert_eq!(player_input_flags(&back), PacketPlayerInputInputs::BACKWARD);

        // Left is the positive strafe direction (vanilla leftImpulse).
        let left = MovementInput {
            strafe: 1.0,
            ..Default::default()
        };
        assert_eq!(player_input_flags(&left), PacketPlayerInputInputs::LEFT);
        let right = MovementInput {
            strafe: -1.0,
            ..Default::default()
        };
        assert_eq!(player_input_flags(&right), PacketPlayerInputInputs::RIGHT);

        // A sprint-jump strafing forward sets exactly its four bits.
        let combo = MovementInput {
            forward: 1.0,
            strafe: 1.0,
            jump: true,
            sprint: true,
            sneak: false,
        };
        assert_eq!(
            player_input_flags(&combo),
            PacketPlayerInputInputs::FORWARD
                | PacketPlayerInputInputs::LEFT
                | PacketPlayerInputInputs::JUMP
                | PacketPlayerInputInputs::SPRINT
        );

        // Sneak maps to SHIFT.
        let sneak = MovementInput {
            sneak: true,
            ..Default::default()
        };
        assert_eq!(player_input_flags(&sneak), PacketPlayerInputInputs::SHIFT);
    }

    #[test]
    fn action_sequence_resets_on_a_fresh_session_reconnect_isolation() {
        // `PlayState` is rebuilt from scratch by `run_play` on every new
        // connection/reconnect attempt (see the module's own `run_play`),
        // so a fresh instance starting back at 1 — not continuing from
        // wherever a previous session's counter left off — is exactly the
        // reconnect isolation this session-scoped counter promises.
        let (event_tx, _rx) = broadcast::channel(crate::minecraft::event::EVENT_CHANNEL_CAPACITY);
        let mut first_session = PlayState::new(event_tx.clone());
        assert_eq!(first_session.next_action_sequence(), 1);
        assert_eq!(first_session.next_action_sequence(), 2);
        assert_eq!(first_session.next_action_sequence(), 3);

        let mut second_session = PlayState::new(event_tx);
        assert_eq!(
            second_session.next_action_sequence(),
            1,
            "a new session's sequence must not continue from the old session's counter"
        );
    }
}
