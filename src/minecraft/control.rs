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
    /// Send a chat message, or — if it starts with `/` — run it as a command.
    /// The play loop turns this into the right serverbound packet.
    Chat(String),
    /// Clear any walk goal and zero all movement input.
    Stop,
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

    /// Sends a chat message, or runs it as a command if it starts with `/`.
    pub fn chat(&self, message: impl Into<String>) -> Result<(), SendError<BotCommand>> {
        self.send(BotCommand::Chat(message.into()))
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
            BotCommand::Chat(_) => {} // sent to the wire by the play loop
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
}
