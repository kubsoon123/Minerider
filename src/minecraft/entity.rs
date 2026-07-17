//! Tracked non-local entities: spawn, movement, rotation, velocity, removal.
//!
//! The store is a pure projection of the clientbound entity packets onto a
//! keyed table of [`Entity`]s. Wire encodings are decoded upstream; this
//! module only applies already-decoded generated packets, so it is
//! unit-tested without any network.

use std::collections::HashMap;

use minerider_protocol::generated::v1_21_4::play::{
    PacketEntityHeadRotation, PacketEntityLook, PacketEntityMoveLook, PacketEntityTeleport,
    PacketEntityVelocity, PacketRelEntityMove, PacketSpawnEntity, PacketSyncEntityPosition,
};

/// Fixed-point unit of the compact relative-move deltas: 1/4096 block.
const DELTA_UNIT: f64 = 4096.0;
/// Fixed-point unit of entity velocity: 1/8000 block per tick.
const VELOCITY_UNIT: f64 = 8000.0;

/// Converts a protocol angle byte (256 steps per full turn) to degrees.
fn angle_to_degrees(a: i8) -> f32 {
    a as f32 * (360.0 / 256.0)
}

/// A non-local entity tracked from clientbound packets.
///
/// Positions are absolute blocks; rotations are degrees; velocity is
/// blocks per tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Entity {
    /// Server-assigned entity id (the table key).
    pub id: i32,
    /// Entity UUID from the spawn packet.
    pub uuid: u128,
    /// Entity type id (index into the generated entity registry).
    pub kind: i32,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    /// Body yaw in degrees.
    pub yaw: f32,
    /// Pitch in degrees.
    pub pitch: f32,
    /// Head yaw in degrees.
    pub head_yaw: f32,
    /// Velocity in blocks per tick.
    pub vx: f64,
    pub vy: f64,
    pub vz: f64,
    pub on_ground: bool,
}

/// Table of tracked entities keyed by entity id.
#[derive(Debug, Clone, Default)]
pub struct EntityStore {
    entities: HashMap<i32, Entity>,
}

impl EntityStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of tracked entities.
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    /// Whether no entities are tracked.
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// Looks up a tracked entity by id.
    pub fn get(&self, id: i32) -> Option<&Entity> {
        self.entities.get(&id)
    }

    /// Iterates over all tracked entities.
    pub fn iter(&self) -> impl Iterator<Item = &Entity> {
        self.entities.values()
    }

    /// Inserts (or replaces) an entity from a `spawn_entity` packet.
    pub fn spawn(&mut self, p: &PacketSpawnEntity) {
        self.entities.insert(
            p.entity_id,
            Entity {
                id: p.entity_id,
                uuid: p.object_uuid,
                kind: p.r#type,
                x: p.x,
                y: p.y,
                z: p.z,
                // Spawn carries pitch, yaw, then head yaw (named `head_pitch`
                // in minecraft-data).
                pitch: angle_to_degrees(p.pitch),
                yaw: angle_to_degrees(p.yaw),
                head_yaw: angle_to_degrees(p.head_pitch),
                vx: p.velocity.x as f64 / VELOCITY_UNIT,
                vy: p.velocity.y as f64 / VELOCITY_UNIT,
                vz: p.velocity.z as f64 / VELOCITY_UNIT,
                on_ground: false,
            },
        );
    }

    /// Removes the given entity ids (`entity_destroy`).
    pub fn destroy(&mut self, ids: &[i32]) {
        for id in ids {
            self.entities.remove(id);
        }
    }

    /// Applies a compact relative move (`rel_entity_move`).
    ///
    /// A delta for an entity we have not spawned is ignored: there is no
    /// absolute base to apply it against.
    pub fn rel_move(&mut self, p: &PacketRelEntityMove) {
        if let Some(e) = self.entities.get_mut(&p.entity_id) {
            e.x += p.d_x as f64 / DELTA_UNIT;
            e.y += p.d_y as f64 / DELTA_UNIT;
            e.z += p.d_z as f64 / DELTA_UNIT;
            e.on_ground = p.on_ground;
        }
    }

    /// Applies a compact relative move plus rotation (`entity_move_look`).
    pub fn move_look(&mut self, p: &PacketEntityMoveLook) {
        if let Some(e) = self.entities.get_mut(&p.entity_id) {
            e.x += p.d_x as f64 / DELTA_UNIT;
            e.y += p.d_y as f64 / DELTA_UNIT;
            e.z += p.d_z as f64 / DELTA_UNIT;
            e.yaw = angle_to_degrees(p.yaw);
            e.pitch = angle_to_degrees(p.pitch);
            e.on_ground = p.on_ground;
        }
    }

    /// Applies a rotation-only update (`entity_look`).
    pub fn look(&mut self, p: &PacketEntityLook) {
        if let Some(e) = self.entities.get_mut(&p.entity_id) {
            e.yaw = angle_to_degrees(p.yaw);
            e.pitch = angle_to_degrees(p.pitch);
            e.on_ground = p.on_ground;
        }
    }

    /// Applies an absolute teleport (`entity_teleport`).
    pub fn teleport(&mut self, p: &PacketEntityTeleport) {
        if let Some(e) = self.entities.get_mut(&p.entity_id) {
            e.x = p.x;
            e.y = p.y;
            e.z = p.z;
            e.yaw = angle_to_degrees(p.yaw);
            e.pitch = angle_to_degrees(p.pitch);
            e.on_ground = p.on_ground;
        }
    }

    /// Applies an absolute position sync with velocity (`sync_entity_position`).
    pub fn sync_position(&mut self, p: &PacketSyncEntityPosition) {
        if let Some(e) = self.entities.get_mut(&p.entity_id) {
            e.x = p.x;
            e.y = p.y;
            e.z = p.z;
            e.vx = p.dx;
            e.vy = p.dy;
            e.vz = p.dz;
            e.yaw = p.yaw;
            e.pitch = p.pitch;
            e.on_ground = p.on_ground;
        }
    }

    /// Applies a velocity update (`entity_velocity`).
    pub fn velocity(&mut self, p: &PacketEntityVelocity) {
        if let Some(e) = self.entities.get_mut(&p.entity_id) {
            e.vx = p.velocity.x as f64 / VELOCITY_UNIT;
            e.vy = p.velocity.y as f64 / VELOCITY_UNIT;
            e.vz = p.velocity.z as f64 / VELOCITY_UNIT;
        }
    }

    /// Applies a head-yaw update (`entity_head_rotation`).
    pub fn head_rotation(&mut self, p: &PacketEntityHeadRotation) {
        if let Some(e) = self.entities.get_mut(&p.entity_id) {
            e.head_yaw = angle_to_degrees(p.head_yaw);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minerider_protocol::generated::v1_21_4::types::Vec3i16;

    fn spawn(id: i32, x: f64, y: f64, z: f64) -> PacketSpawnEntity {
        PacketSpawnEntity {
            entity_id: id,
            object_uuid: 0,
            r#type: 1,
            x,
            y,
            z,
            pitch: 0,
            yaw: 0,
            head_pitch: 0,
            object_data: 0,
            velocity: Vec3i16 { x: 0, y: 0, z: 0 },
        }
    }

    #[test]
    fn spawn_then_get() {
        let mut store = EntityStore::new();
        store.spawn(&spawn(7, 1.0, 2.0, 3.0));
        let e = store.get(7).expect("entity 7 tracked");
        assert_eq!((e.x, e.y, e.z), (1.0, 2.0, 3.0));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn rel_move_adds_fixed_point_delta() {
        let mut store = EntityStore::new();
        store.spawn(&spawn(7, 0.0, 0.0, 0.0));
        // 4096 units == 1 block.
        store.rel_move(&PacketRelEntityMove {
            entity_id: 7,
            d_x: 4096,
            d_y: -8192,
            d_z: 2048,
            on_ground: true,
        });
        let e = store.get(7).unwrap();
        assert_eq!(e.x, 1.0);
        assert_eq!(e.y, -2.0);
        assert_eq!(e.z, 0.5);
        assert!(e.on_ground);
    }

    #[test]
    fn rel_move_on_unknown_entity_is_ignored() {
        let mut store = EntityStore::new();
        store.rel_move(&PacketRelEntityMove {
            entity_id: 99,
            d_x: 4096,
            d_y: 0,
            d_z: 0,
            on_ground: false,
        });
        assert!(store.is_empty());
    }

    #[test]
    fn teleport_sets_absolute() {
        let mut store = EntityStore::new();
        store.spawn(&spawn(7, 0.0, 0.0, 0.0));
        store.teleport(&PacketEntityTeleport {
            entity_id: 7,
            x: 100.0,
            y: 64.0,
            z: -50.0,
            yaw: 64, // quarter turn == 90 degrees
            pitch: 0,
            on_ground: false,
        });
        let e = store.get(7).unwrap();
        assert_eq!((e.x, e.y, e.z), (100.0, 64.0, -50.0));
        assert!((e.yaw - 90.0).abs() < 1e-3);
    }

    #[test]
    fn velocity_uses_8000_unit() {
        let mut store = EntityStore::new();
        store.spawn(&spawn(7, 0.0, 0.0, 0.0));
        store.velocity(&PacketEntityVelocity {
            entity_id: 7,
            velocity: Vec3i16 {
                x: 8000,
                y: 0,
                z: -4000,
            },
        });
        let e = store.get(7).unwrap();
        assert_eq!(e.vx, 1.0);
        assert_eq!(e.vz, -0.5);
    }

    #[test]
    fn destroy_removes() {
        let mut store = EntityStore::new();
        store.spawn(&spawn(1, 0.0, 0.0, 0.0));
        store.spawn(&spawn(2, 0.0, 0.0, 0.0));
        store.destroy(&[1]);
        assert!(store.get(1).is_none());
        assert!(store.get(2).is_some());
        assert_eq!(store.len(), 1);
    }
}
