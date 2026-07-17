//! Bot control surface: an async command channel into the play loop plus the
//! per-tick driver that turns high-level goals into vanilla [`MovementInput`].
//!
//! The play loop owns a [`Controller`]; callers hold a cloneable
//! [`ControlHandle`] and push [`BotCommand`]s into it (`walk_to`, `look`,
//! `set_input`, …). Each tick the loop calls [`Controller::drive`] to fold the
//! current goal and manual overlay into the player's `input` and facing before
//! physics runs. The steering math ([`yaw_toward`]) is pure and unit-tested
//! without any network or async machinery.

use tokio::sync::mpsc::{self, error::SendError};

use crate::minecraft::player::{LocalPlayer, MovementInput};

/// Horizontal distance (blocks) at which a `walk_to` goal counts as reached.
pub const ARRIVAL_RADIUS: f64 = 0.3;

/// Vanilla's protocol-769 maximum for both serverbound chat messages and
/// command text. Java measures this as UTF-16 code units (`String::length`),
/// so Rust byte length or Unicode-scalar count would accept/reject the wrong
/// inputs around non-BMP characters.
pub const MAX_CHAT_UTF16_UNITS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
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
    /// Send an ordinary unsigned chat message. It is never reinterpreted as
    /// a command from its contents.
    Chat(String),
    /// Run a command using the protocol's dedicated command packet. The text
    /// excludes the leading slash.
    Command(String),
    /// Clear any walk goal and zero all movement input.
    Stop,
}

impl BotCommand {
    /// Validates outbound text before it enters a supervised queue or packet
    /// encoder. Non-text commands have no action-specific constraints here.
    pub fn validate(&self) -> Result<(), ActionValidationError> {
        match self {
            Self::Chat(message) => validate_chat(message),
            Self::Command(command) => validate_command(command),
            _ => Ok(()),
        }
    }
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

    /// Sends an ordinary chat message. A leading slash is rejected by the
    /// play-loop validator rather than silently changing packet type.
    pub fn chat(&self, message: impl Into<String>) -> Result<(), SendError<BotCommand>> {
        self.send(BotCommand::Chat(message.into()))
    }

    /// Runs a command using the dedicated packet. `command` omits `/`.
    pub fn command(&self, command: impl Into<String>) -> Result<(), SendError<BotCommand>> {
        self.send(BotCommand::Command(command.into()))
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

/// The play loop's per-tick movement driver.
///
/// Holds a manual input overlay and an optional walk goal; [`drive`](Self::drive)
/// resolves them into the player's `input` and yaw each tick before physics.
#[derive(Debug, Clone, Default)]
pub struct Controller {
    manual: MovementInput,
    goal: Option<(f64, f64)>,
}

impl Controller {
    /// Applies one command to the controller state.
    pub fn apply(&mut self, command: BotCommand) {
        match command {
            BotCommand::SetInput(input) => self.manual = input,
            BotCommand::Look { .. } => {} // handled in drive against the player
            BotCommand::WalkTo { x, z } => self.goal = Some((x, z)),
            BotCommand::Sprint(on) => self.manual.sprint = on,
            BotCommand::Sneak(on) => self.manual.sneak = on,
            BotCommand::Jump(on) => self.manual.jump = on,
            BotCommand::Chat(_) | BotCommand::Command(_) => {} // sent by the play loop
            BotCommand::Stop => {
                self.goal = None;
                self.manual = MovementInput::default();
            }
        }
    }

    /// Applies an absolute look command directly to the player's facing.
    pub fn apply_look(&self, player: &mut LocalPlayer, yaw: f32, pitch: f32) {
        player.position.yaw = yaw;
        player.position.pitch = pitch;
    }

    /// Folds the current goal and manual overlay into the player's `input` and
    /// facing for this tick. A reached walk goal clears itself and stops.
    pub fn drive(&mut self, player: &mut LocalPlayer) {
        let Some((tx, tz)) = self.goal else {
            player.input = self.manual;
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
            return;
        }
        player.position.yaw = yaw_toward(dx, dz);
        let mut input = self.manual;
        input.forward = 1.0;
        input.strafe = 0.0;
        player.input = input;
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
}
