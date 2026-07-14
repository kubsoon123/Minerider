//! Local player state: position, vitals and experience, updated from the
//! authoritative clientbound play packets.
//!
//! Everything here is a pure state transition over a decoded generated
//! packet, so it is unit-tested without any network.

use minerider_protocol::generated::v1_21_4::play::{
    PacketExperience, PacketLogin, PacketPosition, PacketUpdateHealth, PositionUpdateRelatives,
};

/// The client's position and rotation as confirmed by the server.
///
/// Updated only from `synchronize_player_position`; relative flag bits are
/// applied against the previous value, exactly as vanilla does. Velocity and
/// physics belong to a later phase.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayerPosition {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub yaw: f32,
    pub pitch: f32,
}

impl PlayerPosition {
    /// The pre-spawn origin used before the first position sync.
    pub const ORIGIN: PlayerPosition = PlayerPosition {
        x: 0.0,
        y: 0.0,
        z: 0.0,
        yaw: 0.0,
        pitch: 0.0,
    };

    /// Applies a `synchronize_player_position` packet, honoring the
    /// relative-flag bitmask (`PositionUpdateRelatives`).
    pub fn apply(&mut self, packet: &PacketPosition) {
        let flags = &packet.flags;
        if flags.contains(PositionUpdateRelatives::X) {
            self.x += packet.x;
        } else {
            self.x = packet.x;
        }
        if flags.contains(PositionUpdateRelatives::Y) {
            self.y += packet.y;
        } else {
            self.y = packet.y;
        }
        if flags.contains(PositionUpdateRelatives::Z) {
            self.z += packet.z;
        } else {
            self.z = packet.z;
        }
        if flags.contains(PositionUpdateRelatives::YAW) {
            self.yaw += packet.yaw;
        } else {
            self.yaw = packet.yaw;
        }
        if flags.contains(PositionUpdateRelatives::PITCH) {
            self.pitch += packet.pitch;
        } else {
            self.pitch = packet.pitch;
        }
    }
}

/// State of the local (own) player, assembled from clientbound play packets.
///
/// Vitals default to the vanilla spawn values (full health/food) so the
/// struct is meaningful before the first `update_health` arrives.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalPlayer {
    /// Own entity id, as reported by the play `login` packet.
    pub entity_id: Option<i32>,
    /// Server-confirmed position and rotation.
    pub position: PlayerPosition,
    /// Current health in half-hearts (0.0–20.0 on a default server).
    pub health: f32,
    /// Food level (0–20).
    pub food: i32,
    /// Food saturation.
    pub saturation: f32,
    /// Experience bar fill (0.0–1.0).
    pub xp_bar: f32,
    /// Experience level.
    pub xp_level: i32,
    /// Total accumulated experience points.
    pub total_experience: i32,
}

impl LocalPlayer {
    /// A freshly-joined player with vanilla spawn defaults.
    pub fn new() -> Self {
        Self {
            entity_id: None,
            position: PlayerPosition::ORIGIN,
            health: 20.0,
            food: 20,
            saturation: 5.0,
            xp_bar: 0.0,
            xp_level: 0,
            total_experience: 0,
        }
    }

    /// Records the own entity id from the play `login` packet.
    pub fn on_login(&mut self, packet: &PacketLogin) {
        self.entity_id = Some(packet.entity_id);
    }

    /// Applies a `synchronize_player_position` packet.
    pub fn on_position(&mut self, packet: &PacketPosition) {
        self.position.apply(packet);
    }

    /// Applies an `update_health` packet (health, food, saturation).
    pub fn on_health(&mut self, packet: &PacketUpdateHealth) {
        self.health = packet.health;
        self.food = packet.food;
        self.saturation = packet.food_saturation;
    }

    /// Applies an `experience` packet (bar, level, total).
    pub fn on_experience(&mut self, packet: &PacketExperience) {
        self.xp_bar = packet.experience_bar;
        self.xp_level = packet.level;
        self.total_experience = packet.total_experience;
    }

    /// Whether the player currently has any health left.
    pub fn is_alive(&self) -> bool {
        self.health > 0.0
    }
}

impl Default for LocalPlayer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sync(x: f64, y: f64, z: f64, yaw: f32, pitch: f32, flags: u32) -> PacketPosition {
        PacketPosition {
            teleport_id: 1,
            x,
            y,
            z,
            dx: 0.0,
            dy: 0.0,
            dz: 0.0,
            yaw,
            pitch,
            flags: PositionUpdateRelatives(flags),
        }
    }

    #[test]
    fn defaults_are_vanilla_spawn() {
        let p = LocalPlayer::new();
        assert_eq!(p.entity_id, None);
        assert_eq!(p.health, 20.0);
        assert_eq!(p.food, 20);
        assert!(p.is_alive());
    }

    #[test]
    fn absolute_position_replaces() {
        let mut p = LocalPlayer::new();
        p.on_position(&sync(10.0, 64.0, -5.0, 90.0, 0.0, 0));
        assert_eq!(p.position.x, 10.0);
        assert_eq!(p.position.y, 64.0);
        assert_eq!(p.position.z, -5.0);
        assert_eq!(p.position.yaw, 90.0);
    }

    #[test]
    fn relative_position_adds_to_previous() {
        let mut p = LocalPlayer::new();
        p.on_position(&sync(10.0, 64.0, -5.0, 0.0, 0.0, 0));
        // All-relative update: deltas add onto the previous absolute value.
        let all =
            PositionUpdateRelatives::X | PositionUpdateRelatives::Y | PositionUpdateRelatives::Z;
        p.on_position(&sync(1.0, 2.0, 3.0, 0.0, 0.0, all));
        assert_eq!(p.position.x, 11.0);
        assert_eq!(p.position.y, 66.0);
        assert_eq!(p.position.z, -2.0);
    }

    #[test]
    fn health_update_and_death() {
        let mut p = LocalPlayer::new();
        p.on_health(&PacketUpdateHealth {
            health: 0.0,
            food: 3,
            food_saturation: 0.0,
        });
        assert_eq!(p.health, 0.0);
        assert_eq!(p.food, 3);
        assert!(!p.is_alive());
    }

    #[test]
    fn experience_update() {
        let mut p = LocalPlayer::new();
        p.on_experience(&PacketExperience {
            experience_bar: 0.5,
            level: 7,
            total_experience: 100,
        });
        assert_eq!(p.xp_bar, 0.5);
        assert_eq!(p.xp_level, 7);
        assert_eq!(p.total_experience, 100);
    }
}
