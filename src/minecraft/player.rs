//! Local player state: position, vitals and experience, updated from the
//! authoritative clientbound play packets.
//!
//! Everything here is a pure state transition over a decoded generated
//! packet, so it is unit-tested without any network.

use minerider_protocol::generated::v1_21_4::play::{
    MovementFlags, PacketExperience, PacketFlying, PacketLogin, PacketLook, PacketPosition,
    PacketPositionLook, PacketPositionServerbound, PacketUpdateHealth, PositionUpdateRelatives,
};

use crate::core::error::Result;
use crate::minecraft::physics::{collide, Aabb, Vec3};
use crate::minecraft::world::World;

/// Vanilla's maximum number of eligible ticks between position reports.
pub const POSITION_REMINDER_INTERVAL: u32 = 20;

/// Vanilla's squared movement threshold: `(2.0e-4)^2` blocks.
const POSITION_EPSILON_SQUARED: f64 = 2.0e-4 * 2.0e-4;

/// The movement packet selected by vanilla's `LocalPlayer.sendPosition()`.
#[derive(Debug, Clone, PartialEq)]
pub enum MovementPacket {
    PositionLook(PacketPositionLook),
    Position(PacketPositionServerbound),
    Look(PacketLook),
    StatusOnly(PacketFlying),
}

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
    /// Whether vanilla's loading state has declared this world ready.
    pub loaded: bool,
    /// Current ground flag included in every movement packet.
    pub on_ground: bool,
    /// Current horizontal-collision flag included in every movement packet.
    pub horizontal_collision: bool,
    /// Current client-side velocity in blocks per tick.
    pub velocity: Vec3,
    x_last: f64,
    y_last: f64,
    z_last: f64,
    yaw_last: f32,
    pitch_last: f32,
    last_on_ground: bool,
    last_horizontal_collision: bool,
    position_reminder: u32,
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
            loaded: false,
            on_ground: false,
            horizontal_collision: false,
            velocity: Vec3::default(),
            x_last: 0.0,
            y_last: 0.0,
            z_last: 0.0,
            yaw_last: 0.0,
            pitch_last: 0.0,
            last_on_ground: false,
            last_horizontal_collision: false,
            position_reminder: 0,
        }
    }

    /// Records the own entity id from the play `login` packet.
    pub fn on_login(&mut self, packet: &PacketLogin) {
        self.entity_id = Some(packet.entity_id);
        self.loaded = false;
    }

    /// Applies a `synchronize_player_position` packet.
    pub fn on_position(&mut self, packet: &PacketPosition) {
        self.position.apply(packet);
        let flags = &packet.flags;
        self.velocity.x = if flags.contains(PositionUpdateRelatives::DX) {
            self.velocity.x + packet.dx
        } else {
            packet.dx
        };
        self.velocity.y = if flags.contains(PositionUpdateRelatives::DY) {
            self.velocity.y + packet.dy
        } else {
            packet.dy
        };
        self.velocity.z = if flags.contains(PositionUpdateRelatives::DZ) {
            self.velocity.z + packet.dz
        } else {
            packet.dz
        };
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

    /// Marks the initial world load complete and initializes vanilla's last-sent
    /// movement baseline from the current authoritative position.
    pub fn mark_loaded(&mut self) {
        self.loaded = true;
        self.x_last = self.position.x;
        self.y_last = self.position.y;
        self.z_last = self.position.z;
        self.yaw_last = self.position.yaw;
        self.pitch_last = self.position.pitch;
        self.last_on_ground = self.on_ground;
        self.last_horizontal_collision = self.horizontal_collision;
        self.position_reminder = 0;
    }

    /// Runs the no-input, no-effect survival gravity/collision branch for one
    /// vanilla tick. More specialized movement branches are layered on this
    /// collision foundation separately.
    pub fn tick_idle_physics(&mut self, world: &World) -> Result<()> {
        if !self.loaded {
            return Ok(());
        }

        // LivingEntity's normal-air gravity and drag constants in 1.21.4.
        self.velocity.y -= 0.08;
        let aabb = self.bounding_box();
        let swept = Aabb::new(
            aabb.min_x + self.velocity.x.min(0.0),
            aabb.min_y + self.velocity.y.min(0.0),
            aabb.min_z + self.velocity.z.min(0.0),
            aabb.max_x + self.velocity.x.max(0.0),
            aabb.max_y + self.velocity.y.max(0.0),
            aabb.max_z + self.velocity.z.max(0.0),
        );
        let mut boxes = Vec::new();
        world.collision_boxes(swept, &mut boxes)?;
        let result = collide(aabb, self.velocity, &boxes);
        self.position.x += result.movement.x;
        self.position.y += result.movement.y;
        self.position.z += result.movement.z;
        self.on_ground = result.on_ground;
        self.horizontal_collision = result.horizontal_collision;

        if result.movement.x != self.velocity.x {
            self.velocity.x = 0.0;
        }
        if result.movement.y != self.velocity.y {
            self.velocity.y = 0.0;
        }
        if result.movement.z != self.velocity.z {
            self.velocity.z = 0.0;
        }
        self.velocity.x *= 0.91;
        self.velocity.y *= 0.98;
        self.velocity.z *= 0.91;
        Ok(())
    }

    fn bounding_box(&self) -> Aabb {
        const HALF_WIDTH: f64 = 0.3;
        Aabb::new(
            self.position.x - HALF_WIDTH,
            self.position.y,
            self.position.z - HALF_WIDTH,
            self.position.x + HALF_WIDTH,
            self.position.y + 1.8,
            self.position.z + HALF_WIDTH,
        )
    }

    /// Runs one eligible vanilla `sendPosition` tick.
    ///
    /// Packet choice and counter ordering mirror 1.21.4: combined position/look,
    /// position only, look only, then status only. An unchanged position is
    /// reported every 20 eligible ticks.
    pub fn movement_packet(&mut self) -> Option<MovementPacket> {
        if !self.loaded {
            return None;
        }

        let dx = self.position.x - self.x_last;
        let dy = self.position.y - self.y_last;
        let dz = self.position.z - self.z_last;
        self.position_reminder += 1;
        let position_changed = dx * dx + dy * dy + dz * dz > POSITION_EPSILON_SQUARED
            || self.position_reminder >= POSITION_REMINDER_INTERVAL;
        let yaw_delta = self.position.yaw - self.yaw_last;
        let pitch_delta = self.position.pitch - self.pitch_last;
        let rotation_changed = yaw_delta != 0.0 || pitch_delta != 0.0;
        let flags = MovementFlags(
            (u8::from(self.on_ground) * MovementFlags::ON_GROUND)
                | (u8::from(self.horizontal_collision) * MovementFlags::HAS_HORIZONTAL_COLLISION),
        );

        let packet = match (position_changed, rotation_changed) {
            (true, true) => Some(MovementPacket::PositionLook(PacketPositionLook {
                x: self.position.x,
                y: self.position.y,
                z: self.position.z,
                yaw: self.position.yaw,
                pitch: self.position.pitch,
                flags,
            })),
            (true, false) => Some(MovementPacket::Position(PacketPositionServerbound {
                x: self.position.x,
                y: self.position.y,
                z: self.position.z,
                flags,
            })),
            (false, true) => Some(MovementPacket::Look(PacketLook {
                yaw: self.position.yaw,
                pitch: self.position.pitch,
                flags,
            })),
            (false, false)
                if self.on_ground != self.last_on_ground
                    || self.horizontal_collision != self.last_horizontal_collision =>
            {
                Some(MovementPacket::StatusOnly(PacketFlying { flags }))
            }
            (false, false) => None,
        };

        if position_changed {
            self.x_last = self.position.x;
            self.y_last = self.position.y;
            self.z_last = self.position.z;
            self.position_reminder = 0;
        }
        if rotation_changed {
            self.yaw_last = self.position.yaw;
            self.pitch_last = self.position.pitch;
        }
        self.last_on_ground = self.on_ground;
        self.last_horizontal_collision = self.horizontal_collision;
        packet
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

    fn loaded_player() -> LocalPlayer {
        let mut p = LocalPlayer::new();
        p.position = PlayerPosition {
            x: 10.0,
            y: 64.0,
            z: -2.0,
            yaw: 20.0,
            pitch: 5.0,
        };
        p.mark_loaded();
        p
    }

    #[test]
    fn movement_is_silent_before_world_load() {
        let mut p = LocalPlayer::new();
        p.position.x = 10.0;
        assert_eq!(p.movement_packet(), None);
    }

    #[test]
    fn idle_position_reminder_is_exactly_twenty_ticks() {
        let mut p = loaded_player();
        for tick in 1..POSITION_REMINDER_INTERVAL {
            assert_eq!(
                p.movement_packet(),
                None,
                "unexpected packet at tick {tick}"
            );
        }
        assert!(matches!(
            p.movement_packet(),
            Some(MovementPacket::Position(_))
        ));
        assert_eq!(p.movement_packet(), None);
    }

    #[test]
    fn position_threshold_is_strictly_greater_than_vanilla_epsilon() {
        let mut p = loaded_player();
        p.position.x += 2.0e-4;
        assert_eq!(p.movement_packet(), None);
        p.position.x += 1.0e-10;
        assert!(matches!(
            p.movement_packet(),
            Some(MovementPacket::Position(_))
        ));
    }

    #[test]
    fn chooses_position_look_then_position_then_look() {
        let mut p = loaded_player();
        p.position.x += 1.0;
        p.position.yaw += 1.0;
        assert!(matches!(
            p.movement_packet(),
            Some(MovementPacket::PositionLook(_))
        ));

        p.position.z += 1.0;
        assert!(matches!(
            p.movement_packet(),
            Some(MovementPacket::Position(_))
        ));

        p.position.pitch += 1.0;
        assert!(matches!(p.movement_packet(), Some(MovementPacket::Look(_))));
    }

    #[test]
    fn status_only_reports_ground_or_collision_change() {
        let mut p = loaded_player();
        p.on_ground = true;
        let Some(MovementPacket::StatusOnly(packet)) = p.movement_packet() else {
            panic!("expected status-only packet");
        };
        assert!(packet.flags.contains(MovementFlags::ON_GROUND));
        assert!(!packet
            .flags
            .contains(MovementFlags::HAS_HORIZONTAL_COLLISION));

        p.horizontal_collision = true;
        let Some(MovementPacket::StatusOnly(packet)) = p.movement_packet() else {
            panic!("expected collision status-only packet");
        };
        assert!(packet.flags.contains(MovementFlags::ON_GROUND));
        assert!(packet
            .flags
            .contains(MovementFlags::HAS_HORIZONTAL_COLLISION));
    }
}
