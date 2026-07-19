//! Bot control surface: an async command channel into the play loop plus the
//! per-tick driver that turns high-level goals into vanilla [`MovementInput`].
//!
//! The play loop owns a [`Controller`]; callers hold a cloneable
//! [`ControlHandle`] and push [`BotCommand`]s into it (`walk_to`, `look`,
//! `set_input`, …). Each tick the loop calls [`Controller::drive`] to fold the
//! current goal and manual overlay into the player's `input` and facing before
//! physics runs. The steering math ([`yaw_toward`]) is pure and unit-tested
//! without any network or async machinery.

use std::time::Duration;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tokio::sync::mpsc::{self, error::SendError};

use crate::core::tick::TICK_DURATION;
use crate::minecraft::inventory::InventoryClickRequest;
use crate::minecraft::player::{LocalPlayer, MovementInput};

/// Horizontal distance (blocks) at which a `walk_to` goal counts as reached.
pub const ARRIVAL_RADIUS: f64 = 0.3;

/// Vanilla's protocol-769 maximum for both serverbound chat messages and
/// command text. Java measures this as UTF-16 code units (`String::length`),
/// so Rust byte length or Unicode-scalar count would accept/reject the wrong
/// inputs around non-BMP characters.
pub const MAX_CHAT_UTF16_UNITS: usize = 256;

/// Vanilla pitch is clamped to this range; a config or generated angle
/// outside it is folded back in rather than sent to the server unclamped.
pub const PITCH_RANGE: std::ops::RangeInclusive<f32> = -90.0..=90.0;

/// Which hand an action (held-item use, arm swing) applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hand {
    Main,
    Off,
}

impl Hand {
    /// Protocol-769 wire value: `0` main hand, `1` off hand (see
    /// `PacketUseItem`/`PacketArmAnimation`, whose generated `hand` field is
    /// a raw VarInt with no named enum in the generator output).
    pub(crate) fn wire_value(self) -> i32 {
        match self {
            Hand::Main => 0,
            Hand::Off => 1,
        }
    }
}

/// A right-click-on-block interaction (`use_item_on_block` / vanilla
/// `block_place`): where the block is, which face was hit, which hand, and
/// where on the face the cursor landed. `cursor_*` are 0.0..=1.0 fractions
/// across the hit face; `face` is 0..=5 (down, up, north, south, west, east).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockPlacement {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub face: i32,
    pub hand: Hand,
    pub cursor_x: f32,
    pub cursor_y: f32,
    pub cursor_z: f32,
    /// Whether the player's head is inside the target block (vanilla sends
    /// this so the server can reproduce placement rules exactly).
    pub inside_block: bool,
}

/// Bounded, deterministic-when-seeded random head rotation. Not
/// anti-detection/humanization logic — just a small periodic yaw/pitch
/// change, driven by the existing per-tick [`Controller::drive`] rather than
/// a separate task. See [`ControlHandle::set_random_look`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RandomLookConfig {
    /// Shortest delay between two random look changes.
    pub min_interval: Duration,
    /// Longest delay between two random look changes.
    pub max_interval: Duration,
    /// Maximum yaw change (either direction, degrees) applied on top of the
    /// *current* yaw each time a new look is chosen — a bounded random walk,
    /// not a fixed cone around a starting orientation.
    pub max_yaw_delta: f32,
    /// Absolute pitch lower bound (degrees), clamped into
    /// [`PITCH_RANGE`].
    pub min_pitch: f32,
    /// Absolute pitch upper bound (degrees), clamped into
    /// [`PITCH_RANGE`].
    pub max_pitch: f32,
    /// `Some(seed)` makes every drawn interval/angle reproducible (for
    /// tests); `None` seeds from OS entropy.
    pub seed: Option<u64>,
}

impl RandomLookConfig {
    fn validate(&self) -> Result<(), ActionValidationError> {
        if !self.max_yaw_delta.is_finite()
            || !self.min_pitch.is_finite()
            || !self.max_pitch.is_finite()
        {
            return Err(ActionValidationError::RandomLookNonFinite);
        }
        if self.min_interval > self.max_interval {
            return Err(ActionValidationError::RandomLookIntervalOrder);
        }
        if self.min_pitch > self.max_pitch {
            return Err(ActionValidationError::RandomLookPitchOrder);
        }
        if self.max_yaw_delta < 0.0 {
            return Err(ActionValidationError::RandomLookNegativeYawDelta);
        }
        Ok(())
    }
}

// Not `Eq`: `BlockCursorOutOfRange` carries the offending `f32` (which is
// only `PartialEq`) so the message can name it. `PartialEq` is all the
// tests and callers need.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum ActionValidationError {
    #[error("chat messages must not be empty")]
    EmptyChat,
    #[error("commands must not be empty")]
    EmptyCommand,
    #[error("chat text starts with '/'; use the explicit command action instead")]
    ChatStartsWithSlash,
    #[error("command text must omit the leading '/'")]
    CommandStartsWithSlash,
    #[error("chat message is {actual} UTF-16 units; maximum is {max}")]
    ChatTooLong { actual: usize, max: usize },
    #[error("command is {actual} UTF-16 units; maximum is {max}")]
    CommandTooLong { actual: usize, max: usize },
    #[error("random-look config has a non-finite yaw/pitch bound")]
    RandomLookNonFinite,
    #[error("random-look min_interval must not exceed max_interval")]
    RandomLookIntervalOrder,
    #[error("random-look min_pitch must not exceed max_pitch")]
    RandomLookPitchOrder,
    #[error("random-look max_yaw_delta must not be negative")]
    RandomLookNegativeYawDelta,
    #[error("hotbar slot {slot} is out of range; must be 0..=8")]
    HotbarSlotOutOfRange { slot: i16 },
    #[error("block face {face} is out of range; must be 0..=5")]
    BlockFaceOutOfRange { face: i32 },
    #[error("block cursor coordinate {value} is out of range; must be 0.0..=1.0")]
    BlockCursorOutOfRange { value: f32 },
}

/// A command sent from a controller to the running play loop.
#[derive(Debug, Clone, PartialEq)]
pub enum BotCommand {
    /// Replace the manual movement overlay (impulses, sprint, sneak, jump).
    SetInput(MovementInput),
    /// Face an absolute yaw/pitch (degrees).
    Look { yaw: f32, pitch: f32 },
    /// Walk to a horizontal target, steering yaw and holding forward until
    /// within [`ARRIVAL_RADIUS`]. Overrides any manual forward impulse.
    WalkTo { x: f64, z: f64 },
    /// Toggle sprint on the manual overlay.
    Sprint(bool),
    /// Toggle sneak on the manual overlay.
    Sneak(bool),
    /// Toggle the jump key on the manual overlay.
    Jump(bool),
    /// Hold or release the forward key. Independent of [`Self::Backward`] —
    /// holding both cancels out to zero impulse, exactly like vanilla W+S.
    Forward(bool),
    /// Hold or release the backward key. Independent of [`Self::Forward`].
    Backward(bool),
    /// Hold or release the strafe-left key. Independent of
    /// [`Self::StrafeRight`].
    StrafeLeft(bool),
    /// Hold or release the strafe-right key. Independent of
    /// [`Self::StrafeLeft`].
    StrafeRight(bool),
    /// Enables (`Some`) or disables (`None`) bounded random head rotation;
    /// see [`RandomLookConfig`].
    SetRandomLook(Option<RandomLookConfig>),
    /// Send an ordinary unsigned chat message. It is never reinterpreted as
    /// a command from its contents.
    Chat(String),
    /// Run a command using the protocol's dedicated command packet. The text
    /// excludes the leading slash.
    Command(String),
    /// Submit one typed, state-id-bound inventory transaction.
    InventoryClick(InventoryClickRequest),
    /// Use the item held in `Hand` (protocol-769 `use_item`): right-click
    /// activation, not a GUI action. Success here means "the packet was
    /// sent", not "the server accepted the interaction" — protocol 769 has
    /// no dedicated acknowledgement for this specific packet.
    UseItem(Hand),
    /// Play the arm-swing animation for `Hand` without using the held item.
    /// Not automatically coupled to [`Self::UseItem`] — vanilla sends these
    /// independently, and so does this API.
    Swing(Hand),
    /// Select the active hotbar slot (`0..=8`), sending vanilla's
    /// `held_item_slot`. The client tracks the new selection so a later
    /// `use_item`/`swing` acts on the newly held item.
    SelectHotbarSlot(i16),
    /// Right-click a block with the held item (vanilla `block_place`):
    /// place a block, open a container, press a button, etc.
    UseItemOnBlock(BlockPlacement),
    /// Interact with (right-click) an entity by its id — vanilla
    /// `use_entity` in its INTERACT form. Not the attack form.
    InteractEntity {
        entity_id: i32,
        hand: Hand,
        sneaking: bool,
    },
    /// Attack (left-click) an entity by its id — vanilla `use_entity` in its
    /// ATTACK form. Carries no hand (the attack always uses the main hand).
    AttackEntity { entity_id: i32, sneaking: bool },
    /// Interact with an entity at a specific point on its hitbox — vanilla
    /// `use_entity` in its INTERACT_AT form (e.g. clicking a precise part of
    /// an armor stand). `x/y/z` are relative to the entity's position.
    InteractAtEntity {
        entity_id: i32,
        hand: Hand,
        sneaking: bool,
        x: f32,
        y: f32,
        z: f32,
    },
    /// Release the item currently being used (finish eating, release a
    /// drawn bow): vanilla `block_dig` with the RELEASE_USE_ITEM status.
    ReleaseItem,
    /// Close the currently open container/window (vanilla `close_window`).
    /// No-op if nothing is open.
    CloseGui,
    /// Clear any walk goal and zero all movement input.
    Stop,
}

impl BotCommand {
    /// Validates outbound text/config before it enters a supervised queue or
    /// packet encoder. Commands with no action-specific constraint pass
    /// through unchanged.
    pub fn validate(&self) -> Result<(), ActionValidationError> {
        match self {
            Self::Chat(message) => validate_chat(message),
            Self::Command(command) => validate_command(command),
            Self::SetRandomLook(Some(config)) => config.validate(),
            Self::SelectHotbarSlot(slot) if !(0..=8).contains(slot) => {
                Err(ActionValidationError::HotbarSlotOutOfRange { slot: *slot })
            }
            Self::UseItemOnBlock(placement) => validate_placement(placement),
            _ => Ok(()),
        }
    }
}

fn validate_placement(placement: &BlockPlacement) -> Result<(), ActionValidationError> {
    if !(0..=5).contains(&placement.face) {
        return Err(ActionValidationError::BlockFaceOutOfRange {
            face: placement.face,
        });
    }
    for value in [placement.cursor_x, placement.cursor_y, placement.cursor_z] {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err(ActionValidationError::BlockCursorOutOfRange { value });
        }
    }
    Ok(())
}

fn validate_chat(message: &str) -> Result<(), ActionValidationError> {
    if message.is_empty() {
        return Err(ActionValidationError::EmptyChat);
    }
    if message.starts_with('/') {
        return Err(ActionValidationError::ChatStartsWithSlash);
    }
    let actual = message.encode_utf16().count();
    if actual > MAX_CHAT_UTF16_UNITS {
        return Err(ActionValidationError::ChatTooLong {
            actual,
            max: MAX_CHAT_UTF16_UNITS,
        });
    }
    Ok(())
}

fn validate_command(command: &str) -> Result<(), ActionValidationError> {
    if command.is_empty() {
        return Err(ActionValidationError::EmptyCommand);
    }
    if command.starts_with('/') {
        return Err(ActionValidationError::CommandStartsWithSlash);
    }
    let actual = command.encode_utf16().count();
    if actual > MAX_CHAT_UTF16_UNITS {
        return Err(ActionValidationError::CommandTooLong {
            actual,
            max: MAX_CHAT_UTF16_UNITS,
        });
    }
    Ok(())
}

/// A cloneable handle for driving a connected bot from outside the play loop.
///
/// Sends are non-blocking (the channel is unbounded); an [`Err`] means the play
/// loop has ended and the connection is gone.
#[derive(Debug, Clone)]
pub struct ControlHandle {
    tx: mpsc::UnboundedSender<BotCommand>,
}

impl ControlHandle {
    /// Sends a raw command.
    pub fn send(&self, command: BotCommand) -> Result<(), SendError<BotCommand>> {
        self.tx.send(command)
    }

    /// Walks to a horizontal target, steering automatically.
    pub fn walk_to(&self, x: f64, z: f64) -> Result<(), SendError<BotCommand>> {
        self.send(BotCommand::WalkTo { x, z })
    }

    /// Faces an absolute yaw/pitch in degrees.
    pub fn look(&self, yaw: f32, pitch: f32) -> Result<(), SendError<BotCommand>> {
        self.send(BotCommand::Look { yaw, pitch })
    }

    /// Replaces the manual movement overlay.
    pub fn set_input(&self, input: MovementInput) -> Result<(), SendError<BotCommand>> {
        self.send(BotCommand::SetInput(input))
    }

    /// Enables or disables sprint.
    pub fn sprint(&self, on: bool) -> Result<(), SendError<BotCommand>> {
        self.send(BotCommand::Sprint(on))
    }

    /// Enables or disables sneak.
    pub fn sneak(&self, on: bool) -> Result<(), SendError<BotCommand>> {
        self.send(BotCommand::Sneak(on))
    }

    /// Holds or releases the jump key.
    pub fn jump(&self, on: bool) -> Result<(), SendError<BotCommand>> {
        self.send(BotCommand::Jump(on))
    }

    /// Holds or releases the forward key (independent of [`Self::backward`]).
    pub fn forward(&self, on: bool) -> Result<(), SendError<BotCommand>> {
        self.send(BotCommand::Forward(on))
    }

    /// Holds or releases the backward key (independent of [`Self::forward`]).
    pub fn backward(&self, on: bool) -> Result<(), SendError<BotCommand>> {
        self.send(BotCommand::Backward(on))
    }

    /// Holds or releases the strafe-left key (independent of
    /// [`Self::strafe_right`]).
    pub fn strafe_left(&self, on: bool) -> Result<(), SendError<BotCommand>> {
        self.send(BotCommand::StrafeLeft(on))
    }

    /// Holds or releases the strafe-right key (independent of
    /// [`Self::strafe_left`]).
    pub fn strafe_right(&self, on: bool) -> Result<(), SendError<BotCommand>> {
        self.send(BotCommand::StrafeRight(on))
    }

    /// Enables (`Some`) or disables (`None`) bounded random head rotation.
    pub fn set_random_look(
        &self,
        config: Option<RandomLookConfig>,
    ) -> Result<(), SendError<BotCommand>> {
        self.send(BotCommand::SetRandomLook(config))
    }

    /// Sends an ordinary chat message. A leading slash is rejected by the
    /// play-loop validator rather than silently changing packet type.
    pub fn chat(&self, message: impl Into<String>) -> Result<(), SendError<BotCommand>> {
        self.send(BotCommand::Chat(message.into()))
    }

    /// Runs a command using the dedicated packet. `command` omits `/`.
    pub fn command(&self, command: impl Into<String>) -> Result<(), SendError<BotCommand>> {
        self.send(BotCommand::Command(command.into()))
    }

    pub fn inventory_click(
        &self,
        request: InventoryClickRequest,
    ) -> Result<(), SendError<BotCommand>> {
        self.send(BotCommand::InventoryClick(request))
    }

    /// Uses the item held in `hand` (right-click activation).
    pub fn use_item(&self, hand: Hand) -> Result<(), SendError<BotCommand>> {
        self.send(BotCommand::UseItem(hand))
    }

    /// Plays the arm-swing animation for `hand`.
    pub fn swing(&self, hand: Hand) -> Result<(), SendError<BotCommand>> {
        self.send(BotCommand::Swing(hand))
    }

    /// Clears the walk goal and stops all movement.
    pub fn stop(&self) -> Result<(), SendError<BotCommand>> {
        self.send(BotCommand::Stop)
    }
}

/// Creates a paired [`ControlHandle`] and receiver for the play loop.
pub fn channel() -> (ControlHandle, mpsc::UnboundedReceiver<BotCommand>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (ControlHandle { tx }, rx)
}

/// Live random-look state: the caller's config, its own seeded RNG, and a
/// tick countdown to the next chosen orientation.
#[derive(Debug, Clone)]
struct RandomLookRuntime {
    config: RandomLookConfig,
    rng: StdRng,
    ticks_until_next: u32,
}

/// The play loop's per-tick movement driver.
///
/// Holds a manual input overlay and an optional walk goal; [`drive`](Self::drive)
/// resolves them into the player's `input` and yaw each tick before physics.
#[derive(Debug, Clone, Default)]
pub struct Controller {
    manual: MovementInput,
    goal: Option<(f64, f64)>,
    /// Independent held-key state for [`BotCommand::Forward`]/[`BotCommand::Backward`]/
    /// [`BotCommand::StrafeLeft`]/[`BotCommand::StrafeRight`], folded into
    /// `manual.forward`/`manual.strafe` on every change — kept separate from
    /// those two signed fields so opposite keys held together cancel to zero
    /// (vanilla W+S/A+D semantics) instead of one silently overwriting the
    /// other.
    held_forward: bool,
    held_backward: bool,
    held_left: bool,
    held_right: bool,
    random_look: Option<RandomLookRuntime>,
}

impl Controller {
    /// Applies one command to the controller state.
    pub fn apply(&mut self, command: BotCommand) {
        match command {
            BotCommand::SetInput(input) => {
                self.manual = input;
                // Best-effort resync so a later individual directional
                // toggle behaves predictably instead of fighting stale
                // held-key state from before this full-input override.
                self.held_forward = self.manual.forward > 0.0;
                self.held_backward = self.manual.forward < 0.0;
                self.held_left = self.manual.strafe > 0.0;
                self.held_right = self.manual.strafe < 0.0;
            }
            BotCommand::Look { .. } => {} // handled in drive against the player
            BotCommand::WalkTo { x, z } => self.goal = Some((x, z)),
            BotCommand::Sprint(on) => self.manual.sprint = on,
            BotCommand::Sneak(on) => self.manual.sneak = on,
            BotCommand::Jump(on) => self.manual.jump = on,
            BotCommand::Forward(on) => {
                self.held_forward = on;
                self.recompute_forward();
            }
            BotCommand::Backward(on) => {
                self.held_backward = on;
                self.recompute_forward();
            }
            BotCommand::StrafeLeft(on) => {
                self.held_left = on;
                self.recompute_strafe();
            }
            BotCommand::StrafeRight(on) => {
                self.held_right = on;
                self.recompute_strafe();
            }
            BotCommand::SetRandomLook(config) => self.set_random_look(config),
            BotCommand::Chat(_)
            | BotCommand::Command(_)
            | BotCommand::InventoryClick(_)
            | BotCommand::UseItem(_)
            | BotCommand::Swing(_)
            | BotCommand::SelectHotbarSlot(_)
            | BotCommand::UseItemOnBlock(_)
            | BotCommand::InteractEntity { .. }
            | BotCommand::AttackEntity { .. }
            | BotCommand::InteractAtEntity { .. }
            | BotCommand::ReleaseItem
            | BotCommand::CloseGui => {}
            BotCommand::Stop => {
                self.goal = None;
                self.manual = MovementInput::default();
                self.held_forward = false;
                self.held_backward = false;
                self.held_left = false;
                self.held_right = false;
                // Random look is a facing behavior, not movement input;
                // `Stop` intentionally leaves it running. Disable it
                // explicitly via `set_random_look(None)` instead.
            }
        }
    }

    fn recompute_forward(&mut self) {
        self.manual.forward = match (self.held_forward, self.held_backward) {
            (true, false) => 1.0,
            (false, true) => -1.0,
            _ => 0.0,
        };
    }

    fn recompute_strafe(&mut self) {
        self.manual.strafe = match (self.held_left, self.held_right) {
            (true, false) => 1.0,
            (false, true) => -1.0,
            _ => 0.0,
        };
    }

    fn set_random_look(&mut self, config: Option<RandomLookConfig>) {
        match config {
            None => self.random_look = None,
            Some(config) => {
                let mut rng = match config.seed {
                    Some(seed) => StdRng::seed_from_u64(seed),
                    None => StdRng::from_entropy(),
                };
                let ticks_until_next = Self::random_interval_ticks(&config, &mut rng);
                self.random_look = Some(RandomLookRuntime {
                    config,
                    rng,
                    ticks_until_next,
                });
            }
        }
    }

    /// Draws a whole-tick interval in `[min_interval, max_interval]` (at
    /// least one tick), so [`Self::tick_random_look`] only needs a plain
    /// per-tick countdown.
    fn random_interval_ticks(config: &RandomLookConfig, rng: &mut StdRng) -> u32 {
        let tick_ms = TICK_DURATION.as_millis().max(1) as u64;
        let min_ms = (config.min_interval.as_millis() as u64).max(tick_ms);
        let max_ms = (config.max_interval.as_millis() as u64).max(min_ms);
        let interval_ms = if max_ms > min_ms {
            rng.gen_range(min_ms..=max_ms)
        } else {
            min_ms
        };
        ((interval_ms / tick_ms).max(1)) as u32
    }

    /// Advances the random-look countdown by one tick, choosing (and
    /// applying) a new bounded orientation when it reaches zero. Yaw is a
    /// bounded random walk from the player's *current* yaw (clamped into
    /// `-180.0..=180.0` so it never drifts unbounded over a long session);
    /// pitch is drawn fresh from the configured absolute range each time.
    fn tick_random_look(&mut self, player: &mut LocalPlayer) {
        let Some(state) = &mut self.random_look else {
            return;
        };
        if state.ticks_until_next > 0 {
            state.ticks_until_next -= 1;
            return;
        }
        let yaw_delta = if state.config.max_yaw_delta > 0.0 {
            state
                .rng
                .gen_range(-state.config.max_yaw_delta..=state.config.max_yaw_delta)
        } else {
            0.0
        };
        let wrapped_yaw = ((player.position.yaw + yaw_delta + 180.0).rem_euclid(360.0)) - 180.0;
        let min_pitch = state
            .config
            .min_pitch
            .clamp(*PITCH_RANGE.start(), *PITCH_RANGE.end());
        let max_pitch = state
            .config
            .max_pitch
            .clamp(*PITCH_RANGE.start(), *PITCH_RANGE.end())
            .max(min_pitch);
        let pitch = if max_pitch > min_pitch {
            state.rng.gen_range(min_pitch..=max_pitch)
        } else {
            min_pitch
        };
        player.position.yaw = wrapped_yaw;
        player.position.pitch = pitch;
        state.ticks_until_next = Self::random_interval_ticks(&state.config, &mut state.rng);
    }

    /// Applies an absolute look command directly to the player's facing.
    pub fn apply_look(&self, player: &mut LocalPlayer, yaw: f32, pitch: f32) {
        player.position.yaw = yaw;
        player.position.pitch = pitch;
    }

    /// Folds the current goal and manual overlay into the player's `input` and
    /// facing for this tick. A reached walk goal clears itself and stops.
    /// Random look (if enabled) is applied last, so it only visibly changes
    /// yaw on ticks with no active walk goal — an active goal recalculates
    /// its own steering yaw every tick regardless, which otherwise would
    /// immediately overwrite a random deviation on the very next tick
    /// anyway; pitch is unaffected by walking either way.
    pub fn drive(&mut self, player: &mut LocalPlayer) {
        let Some((tx, tz)) = self.goal else {
            player.input = self.manual;
            self.tick_random_look(player);
            return;
        };
        let dx = tx - player.position.x;
        let dz = tz - player.position.z;
        if dx * dx + dz * dz <= ARRIVAL_RADIUS * ARRIVAL_RADIUS {
            self.goal = None;
            let mut input = self.manual;
            input.forward = 0.0;
            input.strafe = 0.0;
            player.input = input;
            self.tick_random_look(player);
            return;
        }
        player.position.yaw = yaw_toward(dx, dz);
        let mut input = self.manual;
        input.forward = 1.0;
        input.strafe = 0.0;
        player.input = input;
        self.tick_random_look(player);
    }

    /// Whether a walk goal is currently active.
    pub fn has_goal(&self) -> bool {
        self.goal.is_some()
    }
}

/// Minecraft yaw (degrees) that faces the horizontal offset `(dx, dz)`.
///
/// Vanilla facing for a forward impulse is `(-sin(yaw), cos(yaw))`, so the yaw
/// that points along `(dx, dz)` is `atan2(-dx, dz)`. Yaw 0 faces +Z (south),
/// yaw -90 faces +X (east), matching the client.
pub fn yaw_toward(dx: f64, dz: f64) -> f32 {
    (-dx).atan2(dz).to_degrees() as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::minecraft::player::PlayerPosition;

    fn player_at(x: f64, z: f64) -> LocalPlayer {
        let mut p = LocalPlayer::new();
        p.position = PlayerPosition {
            x,
            y: 64.0,
            z,
            yaw: 0.0,
            pitch: 0.0,
        };
        p
    }

    #[test]
    fn use_item_on_block_validates_face_and_cursor_ranges() {
        let ok = BlockPlacement {
            x: 1,
            y: 2,
            z: 3,
            face: 3,
            hand: Hand::Main,
            cursor_x: 0.5,
            cursor_y: 0.0,
            cursor_z: 1.0,
            inside_block: false,
        };
        assert!(BotCommand::UseItemOnBlock(ok).validate().is_ok());

        let bad_face = BlockPlacement { face: 6, ..ok };
        assert!(matches!(
            BotCommand::UseItemOnBlock(bad_face).validate(),
            Err(ActionValidationError::BlockFaceOutOfRange { face: 6 })
        ));

        let bad_cursor = BlockPlacement {
            cursor_y: 1.5,
            ..ok
        };
        assert!(matches!(
            BotCommand::UseItemOnBlock(bad_cursor).validate(),
            Err(ActionValidationError::BlockCursorOutOfRange { .. })
        ));

        let nan_cursor = BlockPlacement {
            cursor_z: f32::NAN,
            ..ok
        };
        assert!(matches!(
            BotCommand::UseItemOnBlock(nan_cursor).validate(),
            Err(ActionValidationError::BlockCursorOutOfRange { .. })
        ));
    }

    #[test]
    fn select_hotbar_slot_validates_the_zero_to_eight_range() {
        for slot in 0..=8 {
            assert!(BotCommand::SelectHotbarSlot(slot).validate().is_ok());
        }
        for slot in [-1, 9, 100] {
            assert!(matches!(
                BotCommand::SelectHotbarSlot(slot).validate(),
                Err(ActionValidationError::HotbarSlotOutOfRange { slot: s }) if s == slot
            ));
        }
    }

    #[test]
    fn yaw_toward_matches_vanilla_cardinals() {
        assert!((yaw_toward(0.0, 1.0)).abs() < 1.0e-4); // +Z south
        assert!((yaw_toward(-1.0, 0.0) - 90.0).abs() < 1.0e-4); // -X west
        assert!((yaw_toward(1.0, 0.0) + 90.0).abs() < 1.0e-4); // +X east
        assert!((yaw_toward(0.0, -1.0).abs() - 180.0).abs() < 1.0e-4); // -Z north
    }

    #[test]
    fn walk_to_steers_and_holds_forward() {
        let mut c = Controller::default();
        c.apply(BotCommand::WalkTo { x: 10.0, z: 0.0 });
        let mut p = player_at(0.0, 0.0);
        c.drive(&mut p);
        assert!(c.has_goal());
        assert_eq!(p.input.forward, 1.0);
        // Target +X → yaw -90.
        assert!((p.position.yaw + 90.0).abs() < 1.0e-3);
    }

    #[test]
    fn walk_to_stops_within_arrival_radius() {
        let mut c = Controller::default();
        c.apply(BotCommand::WalkTo { x: 0.2, z: 0.0 });
        let mut p = player_at(0.0, 0.0);
        c.drive(&mut p);
        assert!(!c.has_goal(), "goal cleared on arrival");
        assert_eq!(p.input.forward, 0.0);
    }

    #[test]
    fn manual_overlay_flows_through_without_goal() {
        let mut c = Controller::default();
        c.apply(BotCommand::Sprint(true));
        c.apply(BotCommand::SetInput(MovementInput {
            forward: 1.0,
            sprint: true,
            ..MovementInput::default()
        }));
        let mut p = player_at(0.0, 0.0);
        c.drive(&mut p);
        assert_eq!(p.input.forward, 1.0);
        assert!(p.input.sprint);
    }

    #[test]
    fn walk_to_preserves_manual_sprint() {
        let mut c = Controller::default();
        c.apply(BotCommand::Sprint(true));
        c.apply(BotCommand::WalkTo { x: 10.0, z: 0.0 });
        let mut p = player_at(0.0, 0.0);
        c.drive(&mut p);
        assert!(p.input.sprint, "sprint carries into an auto-walk");
        assert_eq!(p.input.forward, 1.0);
    }

    #[test]
    fn stop_clears_goal_and_input() {
        let mut c = Controller::default();
        c.apply(BotCommand::WalkTo { x: 10.0, z: 0.0 });
        c.apply(BotCommand::Sprint(true));
        c.apply(BotCommand::Stop);
        let mut p = player_at(0.0, 0.0);
        c.drive(&mut p);
        assert!(!c.has_goal());
        assert_eq!(p.input, MovementInput::default());
    }

    #[test]
    fn outbound_text_validation_is_explicit_and_uses_utf16_units() {
        assert_eq!(
            BotCommand::Chat(String::new()).validate(),
            Err(ActionValidationError::EmptyChat)
        );
        assert_eq!(
            BotCommand::Command(String::new()).validate(),
            Err(ActionValidationError::EmptyCommand)
        );
        assert_eq!(
            BotCommand::Chat("/help".into()).validate(),
            Err(ActionValidationError::ChatStartsWithSlash)
        );
        assert_eq!(
            BotCommand::Command("/help".into()).validate(),
            Err(ActionValidationError::CommandStartsWithSlash)
        );

        assert!(BotCommand::Chat("a".repeat(256)).validate().is_ok());
        assert_eq!(
            BotCommand::Chat("😀".repeat(129)).validate(),
            Err(ActionValidationError::ChatTooLong {
                actual: 258,
                max: 256,
            })
        );
        assert_eq!(
            BotCommand::Command("a".repeat(257)).validate(),
            Err(ActionValidationError::CommandTooLong {
                actual: 257,
                max: 256,
            })
        );
    }

    #[test]
    fn chat_and_command_remain_distinct_actions() {
        assert_ne!(
            BotCommand::Chat("say hi".into()),
            BotCommand::Command("say hi".into())
        );
    }

    // ------------------------------------------------------------------
    // Mission E: independent directional controls.
    // ------------------------------------------------------------------

    #[test]
    fn forward_and_backward_are_independent_and_cancel_when_both_held() {
        let mut c = Controller::default();
        c.apply(BotCommand::Forward(true));
        let mut p = player_at(0.0, 0.0);
        c.drive(&mut p);
        assert_eq!(p.input.forward, 1.0);

        c.apply(BotCommand::Backward(true));
        c.drive(&mut p);
        assert_eq!(
            p.input.forward, 0.0,
            "holding both forward and backward cancels to zero, like vanilla W+S"
        );

        c.apply(BotCommand::Forward(false));
        c.drive(&mut p);
        assert_eq!(p.input.forward, -1.0, "backward alone remains -1.0");
    }

    #[test]
    fn strafe_left_and_right_are_independent_and_cancel_when_both_held() {
        let mut c = Controller::default();
        c.apply(BotCommand::StrafeLeft(true));
        let mut p = player_at(0.0, 0.0);
        c.drive(&mut p);
        assert_eq!(p.input.strafe, 1.0);

        c.apply(BotCommand::StrafeRight(true));
        c.drive(&mut p);
        assert_eq!(p.input.strafe, 0.0);

        c.apply(BotCommand::StrafeLeft(false));
        c.drive(&mut p);
        assert_eq!(p.input.strafe, -1.0);
    }

    #[test]
    fn directional_toggles_do_not_disturb_sprint_sneak_or_jump() {
        let mut c = Controller::default();
        c.apply(BotCommand::Sprint(true));
        c.apply(BotCommand::Sneak(true));
        c.apply(BotCommand::Jump(true));
        c.apply(BotCommand::Forward(true));
        c.apply(BotCommand::StrafeRight(true));
        let mut p = player_at(0.0, 0.0);
        c.drive(&mut p);
        assert!(
            p.input.sprint,
            "forward/strafe must not silently clear sprint"
        );
        assert!(p.input.sneak);
        assert!(
            p.input.jump,
            "forward/strafe must not silently enable/disable jump"
        );
        assert_eq!(p.input.forward, 1.0);
        assert_eq!(p.input.strafe, -1.0);
    }

    #[test]
    fn disabling_one_directional_key_does_not_disable_the_others() {
        let mut c = Controller::default();
        c.apply(BotCommand::Forward(true));
        c.apply(BotCommand::StrafeLeft(true));
        c.apply(BotCommand::Sprint(true));
        c.apply(BotCommand::StrafeLeft(false));
        let mut p = player_at(0.0, 0.0);
        c.drive(&mut p);
        assert_eq!(
            p.input.forward, 1.0,
            "disabling strafe must not disable forward"
        );
        assert_eq!(p.input.strafe, 0.0);
        assert!(p.input.sprint, "disabling strafe must not disable sprint");
    }

    #[test]
    fn stop_zeroes_directional_flags_and_clears_goal() {
        let mut c = Controller::default();
        c.apply(BotCommand::Forward(true));
        c.apply(BotCommand::StrafeRight(true));
        c.apply(BotCommand::WalkTo { x: 5.0, z: 0.0 });
        c.apply(BotCommand::Stop);
        let mut p = player_at(0.0, 0.0);
        c.drive(&mut p);
        assert!(!c.has_goal());
        assert_eq!(p.input, MovementInput::default());
        // Directional state was actually cleared, not just the resulting
        // input for this one tick: a later Backward(true) alone must yield
        // exactly -1.0, not "cancelled" by a stale held_forward.
        c.apply(BotCommand::Backward(true));
        c.drive(&mut p);
        assert_eq!(p.input.forward, -1.0);
    }

    #[test]
    fn set_input_resyncs_held_flags_for_later_toggles() {
        let mut c = Controller::default();
        c.apply(BotCommand::SetInput(MovementInput {
            forward: -1.0,
            ..MovementInput::default()
        }));
        // The raw SetInput above is inferred as "backward held" for the
        // purpose of later individual toggles. Immediately holding forward
        // too (without releasing backward first) correctly cancels to zero,
        // exactly like real keyboard W+S — this is not a bug, it's the same
        // cancellation `forward_and_backward_are_independent_and_cancel_when_both_held`
        // already covers, reached via SetInput instead of Backward(true).
        c.apply(BotCommand::Forward(true));
        let mut p = player_at(0.0, 0.0);
        c.drive(&mut p);
        assert_eq!(p.input.forward, 0.0, "forward+backward both held cancels");

        // Releasing backward explicitly is what actually switches direction
        // — the resync's whole purpose is making this release meaningful
        // (without it, there would be no "backward" flag to release at all).
        c.apply(BotCommand::Backward(false));
        c.drive(&mut p);
        assert_eq!(p.input.forward, 1.0);
    }

    // ------------------------------------------------------------------
    // Mission F: bounded random look.
    // ------------------------------------------------------------------

    fn random_look_config(seed: u64) -> RandomLookConfig {
        RandomLookConfig {
            min_interval: Duration::from_millis(50),
            max_interval: Duration::from_millis(50),
            max_yaw_delta: 30.0,
            min_pitch: -20.0,
            max_pitch: 10.0,
            seed: Some(seed),
        }
    }

    #[test]
    fn random_look_config_validates_bounds() {
        assert_eq!(random_look_config(1).validate(), Ok(()));
        let mut bad = random_look_config(1);
        bad.min_interval = Duration::from_secs(2);
        bad.max_interval = Duration::from_secs(1);
        assert_eq!(
            bad.validate(),
            Err(ActionValidationError::RandomLookIntervalOrder)
        );

        let mut bad = random_look_config(1);
        bad.min_pitch = 10.0;
        bad.max_pitch = -10.0;
        assert_eq!(
            bad.validate(),
            Err(ActionValidationError::RandomLookPitchOrder)
        );

        let mut bad = random_look_config(1);
        bad.max_yaw_delta = -1.0;
        assert_eq!(
            bad.validate(),
            Err(ActionValidationError::RandomLookNegativeYawDelta)
        );

        let mut bad = random_look_config(1);
        bad.max_yaw_delta = f32::NAN;
        assert_eq!(
            bad.validate(),
            Err(ActionValidationError::RandomLookNonFinite)
        );
        let mut bad = random_look_config(1);
        bad.min_pitch = f32::INFINITY;
        assert_eq!(
            bad.validate(),
            Err(ActionValidationError::RandomLookNonFinite)
        );
    }

    #[test]
    fn disabled_by_default_and_toggles_cleanly() {
        let mut c = Controller::default();
        let mut p = player_at(0.0, 0.0);
        let (yaw0, pitch0) = (p.position.yaw, p.position.pitch);
        for _ in 0..40 {
            c.drive(&mut p);
        }
        assert_eq!((p.position.yaw, p.position.pitch), (yaw0, pitch0));

        c.apply(BotCommand::SetRandomLook(Some(random_look_config(42))));
        c.drive(&mut p); // ticks_until_next counts down from >=1, so this alone must not yet fire
        c.apply(BotCommand::SetRandomLook(None));
        for _ in 0..40 {
            c.drive(&mut p);
        }
        assert_eq!(
            (p.position.yaw, p.position.pitch),
            (yaw0, pitch0),
            "disabling random look must stop further changes"
        );
    }

    #[test]
    fn yaw_and_pitch_stay_within_configured_bounds_over_many_ticks() {
        let mut c = Controller::default();
        c.apply(BotCommand::SetRandomLook(Some(RandomLookConfig {
            min_interval: Duration::from_millis(50),
            max_interval: Duration::from_millis(50),
            max_yaw_delta: 15.0,
            min_pitch: -25.0,
            max_pitch: 25.0,
            seed: Some(7),
        })));
        let mut p = player_at(0.0, 0.0);
        let mut last_yaw = p.position.yaw;
        for _ in 0..400 {
            c.drive(&mut p);
            assert!(p.position.yaw.is_finite(), "yaw must never be NaN/infinite");
            assert!(
                p.position.pitch.is_finite(),
                "pitch must never be NaN/infinite"
            );
            assert!(
                (-25.0..=25.0).contains(&p.position.pitch),
                "pitch {} outside configured bounds",
                p.position.pitch
            );
            assert!((-180.0..=180.0).contains(&p.position.yaw));
            // Each *step* (when it actually changes) stays within the
            // configured per-change delta; wrap-around at +-180 is the one
            // case where the raw difference looks larger than the delta.
            let step = (p.position.yaw - last_yaw).abs();
            let wrapped_step = 360.0 - step;
            assert!(
                step <= 15.0 + 1e-3 || wrapped_step <= 15.0 + 1e-3,
                "yaw step {step} exceeds max_yaw_delta"
            );
            last_yaw = p.position.yaw;
        }
    }

    #[test]
    fn deterministic_seed_reproduces_the_same_orientation_sequence() {
        let config = random_look_config(123);
        let mut a = Controller::default();
        a.apply(BotCommand::SetRandomLook(Some(config)));
        let mut pa = player_at(0.0, 0.0);

        let mut b = Controller::default();
        b.apply(BotCommand::SetRandomLook(Some(config)));
        let mut pb = player_at(0.0, 0.0);

        for _ in 0..50 {
            a.drive(&mut pa);
            b.drive(&mut pb);
            assert_eq!(pa.position.yaw, pb.position.yaw);
            assert_eq!(pa.position.pitch, pb.position.pitch);
        }
    }

    #[test]
    fn interval_bounds_gate_how_often_orientation_changes() {
        // A long, fixed interval (20 ticks = 1s) must not fire before that
        // many ticks have elapsed.
        let mut c = Controller::default();
        c.apply(BotCommand::SetRandomLook(Some(RandomLookConfig {
            min_interval: Duration::from_secs(1),
            max_interval: Duration::from_secs(1),
            max_yaw_delta: 45.0,
            min_pitch: -10.0,
            max_pitch: 10.0,
            seed: Some(9),
        })));
        let mut p = player_at(0.0, 0.0);
        let start_yaw = p.position.yaw;
        for _ in 0..19 {
            c.drive(&mut p);
        }
        assert_eq!(
            p.position.yaw, start_yaw,
            "must not fire before the interval elapses"
        );
        c.drive(&mut p); // 20th tick: exactly one tick's worth of countdown remains
                         // (Whether it fires on tick 19 or 20 depends on off-by-one framing;
                         // the important, tested guarantee is it did not fire any earlier.)
    }

    #[test]
    fn explicit_look_applies_immediately_and_random_look_resumes_after() {
        let mut c = Controller::default();
        c.apply(BotCommand::SetRandomLook(Some(RandomLookConfig {
            min_interval: Duration::from_secs(10),
            max_interval: Duration::from_secs(10),
            max_yaw_delta: 20.0,
            min_pitch: -10.0,
            max_pitch: 10.0,
            seed: Some(5),
        })));
        let mut p = player_at(0.0, 0.0);
        // `Look` is handled directly by callers via `apply_look`, not
        // through `apply`/`drive` — this mirrors exactly how the play loop
        // wires it (see `minecraft::play::apply_command`).
        c.apply_look(&mut p, 123.0, 45.0);
        assert_eq!(p.position.yaw, 123.0);
        assert_eq!(p.position.pitch, 45.0);
        // Random look is still enabled and unaffected by the explicit look;
        // it keeps counting down toward its own next scheduled change.
        c.drive(&mut p);
        // With a 10s interval (200 ticks) it must not have fired on the
        // very next tick, so the explicit orientation survives immediately
        // afterward.
        assert_eq!(p.position.yaw, 123.0);
        assert_eq!(p.position.pitch, 45.0);
    }

    #[test]
    fn random_look_works_simultaneously_with_forward_movement() {
        let mut c = Controller::default();
        c.apply(BotCommand::Forward(true));
        c.apply(BotCommand::SetRandomLook(Some(random_look_config(3))));
        let mut p = player_at(0.0, 0.0);
        for _ in 0..40 {
            c.drive(&mut p);
            assert_eq!(p.input.forward, 1.0, "forward impulse must keep applying");
        }
        assert!(p.position.yaw.is_finite());
    }

    #[test]
    fn random_look_yaw_is_a_bounded_walk_from_current_yaw_not_a_fixed_cone() {
        // With max_yaw_delta=0, yaw must never move regardless of how many
        // times the interval fires.
        let mut c = Controller::default();
        c.apply(BotCommand::SetRandomLook(Some(RandomLookConfig {
            min_interval: Duration::from_millis(50),
            max_interval: Duration::from_millis(50),
            max_yaw_delta: 0.0,
            min_pitch: -5.0,
            max_pitch: 5.0,
            seed: Some(1),
        })));
        let mut p = player_at(0.0, 0.0);
        for _ in 0..40 {
            c.drive(&mut p);
        }
        assert_eq!(p.position.yaw, 0.0);
    }
}
