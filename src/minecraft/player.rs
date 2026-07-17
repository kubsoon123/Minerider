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
use crate::minecraft::physics::{
    block_friction, collide_with_step, mc_cos, mc_sin, Aabb, Vec3, DEFAULT_FRICTION,
};
use crate::minecraft::world::World;

/// Vanilla's maximum number of eligible ticks between position reports.
pub const POSITION_REMINDER_INTERVAL: u32 = 20;

/// Vanilla's squared movement threshold: `(2.0e-4)^2` blocks.
const POSITION_EPSILON_SQUARED: f64 = 2.0e-4 * 2.0e-4;

/// Gravity acceleration for a normal (non-fluid, non-elytra) living entity.
const GRAVITY: f64 = 0.08;
/// Vertical drag applied every tick after gravity.
const VERTICAL_DRAG: f64 = 0.98;
/// The base horizontal air/ground drag factor multiplied into block friction.
const BASE_DRAG: f64 = 0.91;
/// `MOVEMENT_SPEED` attribute for a default player.
const WALK_SPEED: f64 = 0.1;
/// Walking speed after the +30% `SPEED_MODIFIER_SPRINTING` multiplier.
const SPRINT_SPEED: f64 = 0.130_000_01;
/// `getFlyingSpeed` while airborne: walking and sprinting variants.
const AIR_SPEED: f64 = 0.02;
const AIR_SPEED_SPRINT: f64 = 0.025_999_999;
/// `SNEAKING_SPEED` attribute: impulse scale while crouching.
const SNEAK_FACTOR: f64 = 0.3;
/// Impulse scale applied to raw movement keys (`xxa`/`zza = impulse * 0.98`).
const INPUT_SCALE: f64 = 0.98;
/// `getJumpPower` for a default player with no jump-boost effect.
const JUMP_POWER: f64 = 0.42;
/// Sprint-jump horizontal boost magnitude.
const SPRINT_JUMP_BOOST: f64 = 0.2;
/// Cooldown ticks after a ground jump before another is allowed.
const JUMP_DELAY_TICKS: u8 = 10;
/// Per-axis velocity magnitude below which vanilla zeroes the component.
const VELOCITY_EPSILON: f64 = 0.003;
/// A player's `maxUpStep`: the height it auto-climbs without jumping.
const STEP_HEIGHT: f64 = 0.6;

/// Per-tick movement intent, the Mineflayer-style control surface. Impulses are
/// vanilla key states in `-1.0..=1.0` (forward and left positive); the physics
/// step turns them into acceleration exactly as the vanilla client does.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MovementInput {
    /// Forward (+1) / backward (-1) impulse.
    pub forward: f32,
    /// Left (+1) / right (-1) impulse, matching vanilla `leftImpulse`.
    pub strafe: f32,
    /// Whether the jump key is held.
    pub jump: bool,
    /// Whether the player is sprinting.
    pub sprint: bool,
    /// Whether the player is sneaking (crouching).
    pub sneak: bool,
}

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
    /// Movement intent applied on the next physics tick.
    pub input: MovementInput,
    /// Remaining cooldown ticks before another ground jump is allowed.
    jump_delay: u8,
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
            input: MovementInput::default(),
            jump_delay: 0,
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

    /// Resets transient physics state for a fresh life after a `respawn`
    /// packet. Position/health/food update from the packets that accompany a
    /// respawn (a fresh `synchronize_player_position` and `update_health`);
    /// this only clears carried-over velocity/ground state and drops
    /// `loaded` so the play loop re-enters the normal readiness gate
    /// (wait for position + chunk, then resend `player_loaded`).
    pub fn on_respawn(&mut self) {
        self.loaded = false;
        self.velocity = Vec3::default();
        self.on_ground = false;
        self.horizontal_collision = false;
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

    /// Applies an `entity_velocity` packet addressed to this player's own
    /// entity id: vanilla's knockback/explosion path. The packet is an
    /// absolute velocity (not a delta) in 1/8000-block-per-tick units; the
    /// caller (`play::apply_state_packet`) is responsible for the entity-id
    /// match, since a non-matching id belongs to the remote `EntityStore`.
    pub fn apply_velocity(&mut self, velocity: Vec3) {
        self.velocity = velocity;
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

    /// Runs one vanilla physics tick (`aiStep` + `travel`) for the normal
    /// (non-fluid, non-elytra, no-effect) survival case, applying the current
    /// [`MovementInput`]. With default (empty) input this reduces to the vanilla
    /// idle gravity/collision behavior.
    ///
    /// Ordering follows 1.21.4 exactly: small velocities are zeroed, a ground
    /// jump is applied, horizontal input acceleration is added, the move is
    /// clipped against the world, and only then is gravity applied and drag
    /// multiplied in. Getting this order right is what makes the resulting
    /// position/`on_ground` stream match a server's own re-simulation.
    pub fn tick_physics(&mut self, world: &World) -> Result<()> {
        if !self.loaded {
            return Ok(());
        }
        let input = self.input;

        // aiStep: zero any per-axis velocity below the vanilla threshold.
        clamp_epsilon(&mut self.velocity.x);
        clamp_epsilon(&mut self.velocity.y);
        clamp_epsilon(&mut self.velocity.z);

        // aiStep: a ground jump, evaluated before travel so this tick's
        // friction/speed still use the grounded state.
        if self.jump_delay > 0 {
            self.jump_delay -= 1;
        }
        if input.jump && self.on_ground && self.jump_delay == 0 {
            self.velocity.y = JUMP_POWER;
            if input.sprint {
                let yaw = self.position.yaw * (std::f32::consts::PI / 180.0);
                self.velocity.x -= (mc_sin(yaw) as f64) * SPRINT_JUMP_BOOST;
                self.velocity.z += (mc_cos(yaw) as f64) * SPRINT_JUMP_BOOST;
            }
            self.jump_delay = JUMP_DELAY_TICKS;
        }

        // travel: friction and speed use the ground state coming into the tick,
        // sampled from the block the player is standing on (ice/slime differ).
        let grounded = self.on_ground;
        let friction = self.friction_below(world);
        let horizontal_drag = if grounded {
            friction * BASE_DRAG
        } else {
            BASE_DRAG
        };
        let speed = if grounded {
            let base = if input.sprint {
                SPRINT_SPEED
            } else {
                WALK_SPEED
            };
            base * (0.216_000_02 / (friction * friction * friction))
        } else if input.sprint {
            AIR_SPEED_SPRINT
        } else {
            AIR_SPEED
        };

        // moveRelative: rotate the (sneak-scaled) impulse into world velocity.
        let sneak = if input.sneak { SNEAK_FACTOR } else { 1.0 };
        let (ax, az) = input_acceleration(
            f64::from(input.strafe) * sneak * INPUT_SCALE,
            f64::from(input.forward) * sneak * INPUT_SCALE,
            speed,
            self.position.yaw,
        );
        self.velocity.x += ax;
        self.velocity.z += az;

        // move(SELF): clip the requested velocity against the world, with the
        // query expanded upward by the step height so auto step-up can probe it.
        let aabb = self.bounding_box();
        let swept = Aabb::new(
            aabb.min_x + self.velocity.x.min(0.0),
            aabb.min_y + self.velocity.y.min(0.0),
            aabb.min_z + self.velocity.z.min(0.0),
            aabb.max_x + self.velocity.x.max(0.0),
            aabb.max_y + self.velocity.y.max(0.0) + STEP_HEIGHT,
            aabb.max_z + self.velocity.z.max(0.0),
        );
        let mut boxes = Vec::new();
        if !world.collision_boxes(swept, &mut boxes)? {
            // Terrain this move would touch isn't loaded yet (e.g. a gap at
            // the edge of view distance, or right after a server teleport).
            // Skip physics for this tick and retry once the chunk streams in,
            // rather than treating a momentary streaming gap as fatal.
            return Ok(());
        }
        let result = collide_with_step(aabb, self.velocity, STEP_HEIGHT, grounded, &boxes);
        self.position.x += result.movement.x;
        self.position.y += result.movement.y;
        self.position.z += result.movement.z;
        self.on_ground = result.on_ground;
        self.horizontal_collision = result.horizontal_collision;

        // move() zeroes velocity on any clipped axis.
        if result.movement.x != self.velocity.x {
            self.velocity.x = 0.0;
        }
        if result.movement.y != self.velocity.y {
            self.velocity.y = 0.0;
        }
        if result.movement.z != self.velocity.z {
            self.velocity.z = 0.0;
        }

        // travel tail: gravity is applied to the post-move velocity, then drag.
        self.velocity.y -= GRAVITY;
        self.velocity.y *= VERTICAL_DRAG;
        self.velocity.x *= horizontal_drag;
        self.velocity.z *= horizontal_drag;
        Ok(())
    }

    /// The friction of the block the player is standing on. Vanilla samples the
    /// block at `floor(feetY - 0.2)` below the player; an unloaded block falls
    /// back to the default friction.
    fn friction_below(&self, world: &World) -> f64 {
        let bx = self.position.x.floor() as i32;
        let by = (self.position.y - 0.2).floor() as i32;
        let bz = self.position.z.floor() as i32;
        match world.block_state(bx, by, bz) {
            Some(state) => block_friction(state),
            None => DEFAULT_FRICTION,
        }
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

/// Vanilla `aiStep` per-axis clamp: any component below the epsilon is zeroed.
fn clamp_epsilon(component: &mut f64) {
    if component.abs() < VELOCITY_EPSILON {
        *component = 0.0;
    }
}

/// Vanilla `moveRelative`/`getInputVector`: normalize the impulse when it
/// exceeds unit length, scale by `speed`, then rotate by yaw using the `Mth`
/// sine table. Returns the `(x, z)` velocity contribution.
fn input_acceleration(strafe: f64, forward: f64, speed: f64, yaw_degrees: f32) -> (f64, f64) {
    let length_squared = strafe * strafe + forward * forward;
    if length_squared < 1.0e-7 {
        return (0.0, 0.0);
    }
    let (mut sx, mut fz) = (strafe, forward);
    if length_squared > 1.0 {
        let inv_length = 1.0 / length_squared.sqrt();
        sx *= inv_length;
        fz *= inv_length;
    }
    sx *= speed;
    fz *= speed;
    let yaw = yaw_degrees * (std::f32::consts::PI / 180.0);
    let sin = f64::from(mc_sin(yaw));
    let cos = f64::from(mc_cos(yaw));
    (sx * cos - fz * sin, fz * cos + sx * sin)
}

#[cfg(test)]
mod tests {
    use minerider_protocol::buffer::PacketWriter;
    use minerider_protocol::generated::v1_21_4::play::PacketMapChunk;
    use minerider_protocol::nbt::Nbt;

    use super::*;
    use crate::minecraft::configuration::DimensionType;
    use crate::minecraft::world::World;

    /// Global state id of `minecraft:stone` (a full collision cube).
    const STONE: u32 = 1;

    fn dimension() -> DimensionType {
        DimensionType {
            key: "minecraft:overworld".into(),
            min_y: -64,
            height: 384,
            logical_height: 384,
            coordinate_scale: 1.0,
            ultrawarm: false,
            has_ceiling: false,
        }
    }

    fn single_section(block: u32) -> Vec<u8> {
        let mut out = PacketWriter::new();
        out.put_i16(if block == 0 { 0 } else { 4096 });
        out.put_u8(0);
        out.put_varint(block as i32);
        out.put_varint(0);
        out.put_u8(0);
        out.put_varint(0);
        out.put_varint(0);
        out.into_inner().to_vec()
    }

    fn chunk_packet(data: Vec<u8>) -> PacketMapChunk {
        PacketMapChunk {
            x: 0,
            z: 0,
            heightmaps: Nbt::Compound(vec![]),
            chunk_data: data,
            block_entities: vec![],
            sky_light_mask: vec![],
            block_light_mask: vec![],
            empty_sky_light_mask: vec![],
            empty_block_light_mask: vec![],
            sky_light: vec![],
            block_light: vec![],
        }
    }

    /// A world whose chunk (0,0) is solid `floor_block` in section 7
    /// (y 48..=63) and air elsewhere: a flat floor with its surface at y = 64.
    fn world_with_floor(floor_block: u32) -> World {
        let mut data = Vec::new();
        for section in 0..24 {
            data.extend(single_section(if section == 7 { floor_block } else { 0 }));
        }
        let mut world = World::new(dimension());
        world.insert_chunk(&chunk_packet(data)).unwrap();
        world
    }

    /// A stone-floored world (default 0.6 friction).
    fn flat_world() -> World {
        world_with_floor(STONE)
    }

    /// Blocks coasted along +Z after movement input is released, a proxy for
    /// how slippery the floor is.
    fn slide_after_release(floor_block: u32) -> f64 {
        let world = world_with_floor(floor_block);
        let mut p = standing_player(0.0);
        p.input = MovementInput {
            forward: 1.0,
            ..MovementInput::default()
        };
        for _ in 0..25 {
            p.tick_physics(&world).unwrap();
        }
        p.input = MovementInput::default();
        let start = p.position.z;
        for _ in 0..40 {
            p.tick_physics(&world).unwrap();
        }
        p.position.z - start
    }

    /// A world whose chunk (0,0) is entirely air: free-fall with no floor.
    fn air_world() -> World {
        let mut data = Vec::new();
        for _ in 0..24 {
            data.extend(single_section(0));
        }
        let mut world = World::new(dimension());
        world.insert_chunk(&chunk_packet(data)).unwrap();
        world
    }

    fn standing_player(yaw: f32) -> LocalPlayer {
        let mut p = LocalPlayer::new();
        p.position = PlayerPosition {
            x: 0.5,
            y: 64.0,
            z: 0.5,
            yaw,
            pitch: 0.0,
        };
        p.mark_loaded();
        p
    }

    /// Steady-state per-tick horizontal displacement after the velocity has
    /// converged, driving straight forward (+Z at yaw 0) on flat ground.
    fn steady_forward_speed(input: MovementInput) -> f64 {
        let world = flat_world();
        let mut p = standing_player(0.0);
        p.input = input;
        // Converges within ~20 ticks; stop well short of the chunk boundary.
        for _ in 0..40 {
            p.tick_physics(&world).unwrap();
        }
        let before = p.position.z;
        p.tick_physics(&world).unwrap();
        p.position.z - before
    }

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
    fn on_respawn_clears_carried_over_physics_state() {
        let mut p = LocalPlayer::new();
        p.mark_loaded();
        p.velocity = Vec3 {
            x: 1.0,
            y: -3.92,
            z: 1.0,
        };
        p.on_ground = true;
        p.horizontal_collision = true;

        p.on_respawn();

        assert!(!p.loaded, "readiness gate resets so the load flow re-runs");
        assert_eq!(p.velocity, Vec3::default(), "no carried-over fall velocity");
        assert!(!p.on_ground);
        assert!(!p.horizontal_collision);
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

    #[test]
    fn apply_velocity_sets_knockback_absolutely() {
        // entity_velocity is an absolute set, not a delta (unlike synchronize
        // position's dx/dy/dz relative flags).
        let mut p = LocalPlayer::new();
        p.velocity = Vec3 {
            x: 0.1,
            y: 0.0,
            z: 0.1,
        };
        p.apply_velocity(Vec3 {
            x: -0.4,
            y: 0.36,
            z: 0.2,
        });
        assert_eq!(
            p.velocity,
            Vec3 {
                x: -0.4,
                y: 0.36,
                z: 0.2
            }
        );
    }

    #[test]
    fn knocked_back_player_actually_moves_next_tick() {
        // A hit that sets upward+lateral velocity should displace the player
        // on the very next physics tick, exactly like vanilla knockback.
        let world = air_world();
        let mut p = LocalPlayer::new();
        p.position = PlayerPosition {
            x: 0.5,
            y: 300.0,
            z: 0.5,
            yaw: 0.0,
            pitch: 0.0,
        };
        p.mark_loaded();
        p.apply_velocity(Vec3 {
            x: 0.4,
            y: 0.36,
            z: 0.0,
        });
        p.tick_physics(&world).unwrap();
        assert!(p.position.x > 0.5, "knockback moved the player in +x");
        assert!(p.position.y > 300.0, "knockback launched the player upward");
    }

    #[test]
    fn free_fall_ordering_matches_vanilla_recurrence() {
        // Gravity is applied *after* the move, so the first airborne tick from
        // rest displaces nothing and only then acquires -0.08 * 0.98 velocity.
        // An independent recurrence must reproduce position and velocity
        // exactly; a gravity-before-move ordering would diverge on tick one.
        let world = air_world();
        let mut p = LocalPlayer::new();
        p.position = PlayerPosition {
            x: 0.5,
            y: 300.0,
            z: 0.5,
            yaw: 0.0,
            pitch: 0.0,
        };
        p.mark_loaded();

        let mut ref_pos = 300.0_f64;
        let mut ref_vel = 0.0_f64;
        for tick in 0..25 {
            p.tick_physics(&world).unwrap();
            ref_pos += ref_vel; // displacement uses pre-gravity velocity
            ref_vel = (ref_vel - GRAVITY) * VERTICAL_DRAG;
            assert!(
                (p.position.y - ref_pos).abs() < 1.0e-9,
                "position diverged at tick {tick}: {} vs {ref_pos}",
                p.position.y
            );
            assert!(
                (p.velocity.y - ref_vel).abs() < 1.0e-9,
                "velocity diverged at tick {tick}: {} vs {ref_vel}",
                p.velocity.y
            );
        }
        // First tick displaced nothing; velocity is exactly -0.0784.
        assert!((p.velocity.y - ref_vel).abs() < 1.0e-9);
    }

    #[test]
    fn free_fall_approaches_vanilla_terminal_velocity() {
        // Iterating the ordering to its fixed point yields -3.92 blocks/tick,
        // vanilla's terminal velocity, not the -4.0 a mis-ordered gravity gives.
        let mut vel = 0.0_f64;
        for _ in 0..2000 {
            vel = (vel - GRAVITY) * VERTICAL_DRAG;
        }
        assert!((vel - (-3.92)).abs() < 1.0e-6, "terminal velocity {vel}");
    }

    #[test]
    fn resting_player_becomes_grounded_then_holds_position() {
        // From rest on a floor the first move detects no ground (velocity.y is
        // zero), matching vanilla; the next tick lands and stays put.
        let world = flat_world();
        let mut p = standing_player(0.0);
        assert!(!p.on_ground);

        p.tick_physics(&world).unwrap();
        assert!(!p.on_ground, "first tick from rest is not yet grounded");

        p.tick_physics(&world).unwrap();
        assert!(p.on_ground, "second tick lands on the floor");
        assert!((p.position.y - 64.0).abs() < 1.0e-9);

        for _ in 0..40 {
            p.tick_physics(&world).unwrap();
            assert!(p.on_ground);
            assert!(
                (p.position.y - 64.0).abs() < 1.0e-9,
                "rests exactly on floor"
            );
        }
    }

    #[test]
    fn walking_reaches_vanilla_ground_speed() {
        // ~4.317 m/s = 0.2159 blocks/tick.
        let speed = steady_forward_speed(MovementInput {
            forward: 1.0,
            ..MovementInput::default()
        });
        assert!((0.213..=0.218).contains(&speed), "walk speed {speed}");
    }

    #[test]
    fn sprinting_reaches_vanilla_ground_speed() {
        // ~5.612 m/s = 0.2806 blocks/tick.
        let speed = steady_forward_speed(MovementInput {
            forward: 1.0,
            sprint: true,
            ..MovementInput::default()
        });
        assert!((0.278..=0.283).contains(&speed), "sprint speed {speed}");
    }

    #[test]
    fn sneaking_scales_movement_to_vanilla_crouch_speed() {
        // The 0.3 sneak factor drops steady speed to ~0.0648 blocks/tick.
        let speed = steady_forward_speed(MovementInput {
            forward: 1.0,
            sneak: true,
            ..MovementInput::default()
        });
        assert!((0.062..=0.068).contains(&speed), "sneak speed {speed}");
    }

    #[test]
    fn jump_reaches_vanilla_apex_and_lands() {
        // A held jump peaks ~1.2522 blocks above the floor, then returns.
        let world = flat_world();
        let mut p = standing_player(0.0);
        // Settle onto the floor so the jump fires from a grounded tick.
        p.tick_physics(&world).unwrap();
        p.tick_physics(&world).unwrap();
        assert!(p.on_ground);

        // Trigger a single jump, then release so it does not bunny-hop.
        p.input = MovementInput {
            jump: true,
            ..MovementInput::default()
        };
        p.tick_physics(&world).unwrap();
        p.input = MovementInput::default();
        let mut apex = p.position.y;
        for _ in 0..30 {
            p.tick_physics(&world).unwrap();
            apex = apex.max(p.position.y);
        }
        let height = apex - 64.0;
        assert!((1.249..=1.2523).contains(&height), "jump height {height}");
        assert!(p.on_ground, "returns to the ground");
        assert!(
            (p.position.y - 64.0).abs() < 1.0e-9,
            "lands back on the floor"
        );
    }

    #[test]
    fn ice_is_slippery_relative_to_stone() {
        // Ice (0.98 friction) retains far more momentum after input stops than
        // stone (0.6), so the coast distance is several times longer.
        const ICE: u32 = 5949;
        let ice = slide_after_release(ICE);
        let stone = slide_after_release(STONE);
        assert!(
            ice > stone * 3.0,
            "ice {ice} should out-slide stone {stone}"
        );
    }

    #[test]
    fn controller_walks_bot_to_target_through_physics() {
        use crate::minecraft::control::{BotCommand, Controller};

        let world = flat_world();
        let mut p = standing_player(0.0);
        let mut controller = Controller::default();
        controller.apply(BotCommand::WalkTo { x: 0.5, z: 8.0 });

        for _ in 0..120 {
            controller.drive(&mut p);
            p.tick_physics(&world).unwrap();
            if !controller.has_goal() {
                break;
            }
        }
        assert!(!controller.has_goal(), "reached the target");
        assert!(
            (p.position.z - 8.0).abs() < 0.5,
            "arrived near z=8: {}",
            p.position.z
        );
        assert!(
            (p.position.x - 0.5).abs() < 0.2,
            "held the lane: {}",
            p.position.x
        );
    }

    #[test]
    fn forward_input_moves_along_facing() {
        // At yaw 0 the player faces +Z, so forward input accelerates +Z only.
        let world = flat_world();
        let mut p = standing_player(0.0);
        p.input = MovementInput {
            forward: 1.0,
            ..MovementInput::default()
        };
        for _ in 0..5 {
            p.tick_physics(&world).unwrap();
        }
        assert!(p.position.z > 0.5, "advanced along +Z");
        assert!((p.position.x - 0.5).abs() < 1.0e-9, "no lateral drift");
    }
}
