//! Vanilla-aligned player AABB collision primitives.

/// Axis-aligned bounding box in world or block-local coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aabb {
    pub min_x: f64,
    pub min_y: f64,
    pub min_z: f64,
    pub max_x: f64,
    pub max_y: f64,
    pub max_z: f64,
}

impl Aabb {
    pub const fn new(
        min_x: f64,
        min_y: f64,
        min_z: f64,
        max_x: f64,
        max_y: f64,
        max_z: f64,
    ) -> Self {
        Self {
            min_x,
            min_y,
            min_z,
            max_x,
            max_y,
            max_z,
        }
    }

    pub fn moved(self, x: f64, y: f64, z: f64) -> Self {
        Self::new(
            self.min_x + x,
            self.min_y + y,
            self.min_z + z,
            self.max_x + x,
            self.max_y + y,
            self.max_z + z,
        )
    }

    fn intersects_xz(self, other: Self) -> bool {
        self.max_x > other.min_x
            && self.min_x < other.max_x
            && self.max_z > other.min_z
            && self.min_z < other.max_z
    }

    fn intersects_yz(self, other: Self) -> bool {
        self.max_y > other.min_y
            && self.min_y < other.max_y
            && self.max_z > other.min_z
            && self.min_z < other.max_z
    }

    fn intersects_xy(self, other: Self) -> bool {
        self.max_x > other.min_x
            && self.min_x < other.max_x
            && self.max_y > other.min_y
            && self.min_y < other.max_y
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Vec3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CollisionResult {
    pub movement: Vec3,
    pub horizontal_collision: bool,
    pub vertical_collision: bool,
    pub on_ground: bool,
}

/// Clips requested movement against the supplied world-space voxel boxes.
/// Vanilla resolves Y first, then one horizontal axis followed by the other;
/// the longer requested horizontal component is resolved first.
pub fn collide(aabb: Aabb, movement: Vec3, boxes: &[Aabb]) -> CollisionResult {
    let requested = movement;
    let mut moved = aabb;

    let mut y = movement.y;
    for &shape in boxes {
        if moved.intersects_xz(shape) {
            if y > 0.0 && moved.max_y <= shape.min_y {
                y = y.min(shape.min_y - moved.max_y);
            } else if y < 0.0 && moved.min_y >= shape.max_y {
                y = y.max(shape.max_y - moved.min_y);
            }
        }
    }
    moved = moved.moved(0.0, y, 0.0);

    let (x, z) = if movement.x.abs() < movement.z.abs() {
        let z = clip_z(moved, movement.z, boxes);
        moved = moved.moved(0.0, 0.0, z);
        let x = clip_x(moved, movement.x, boxes);
        (x, z)
    } else {
        let x = clip_x(moved, movement.x, boxes);
        moved = moved.moved(x, 0.0, 0.0);
        let z = clip_z(moved, movement.z, boxes);
        (x, z)
    };

    let vertical_collision = y != requested.y;
    CollisionResult {
        movement: Vec3 { x, y, z },
        horizontal_collision: x != requested.x || z != requested.z,
        vertical_collision,
        on_ground: vertical_collision && requested.y < 0.0,
    }
}

fn clip_x(aabb: Aabb, mut movement: f64, boxes: &[Aabb]) -> f64 {
    for &shape in boxes {
        if aabb.intersects_yz(shape) {
            if movement > 0.0 && aabb.max_x <= shape.min_x {
                movement = movement.min(shape.min_x - aabb.max_x);
            } else if movement < 0.0 && aabb.min_x >= shape.max_x {
                movement = movement.max(shape.max_x - aabb.min_x);
            }
        }
    }
    movement
}

fn clip_z(aabb: Aabb, mut movement: f64, boxes: &[Aabb]) -> f64 {
    for &shape in boxes {
        if aabb.intersects_xy(shape) {
            if movement > 0.0 && aabb.max_z <= shape.min_z {
                movement = movement.min(shape.min_z - aabb.max_z);
            } else if movement < 0.0 && aabb.min_z >= shape.max_z {
                movement = movement.max(shape.max_z - aabb.min_z);
            }
        }
    }
    movement
}

/// Returns the exact block-local collision boxes for a 1.21.4 global state id.
pub fn collision_boxes(state_id: u32) -> Option<&'static [Aabb]> {
    let shape = *super::collision_data::STATE_SHAPES.get(state_id as usize)? as usize;
    let &(start, count) = super::collision_data::SHAPE_RANGES.get(shape)?;
    super::collision_data::SHAPE_BOXES.get(start as usize..start as usize + count as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn air_and_stone_shapes_match_registry() {
        assert_eq!(collision_boxes(0), Some(&[][..]));
        assert_eq!(
            collision_boxes(1),
            Some(&[Aabb::new(0.0, 0.0, 0.0, 1.0, 1.0, 1.0)][..])
        );
    }

    #[test]
    fn every_global_state_resolves_to_a_shape() {
        for state in 0..super::super::collision_data::STATE_COUNT {
            assert!(collision_boxes(state as u32).is_some(), "state {state}");
        }
    }

    #[test]
    fn falling_player_lands_on_full_block() {
        let player = Aabb::new(0.2, 1.1, 0.2, 0.8, 2.9, 0.8);
        let floor = [Aabb::new(0.0, 0.0, 0.0, 1.0, 1.0, 1.0)];
        let result = collide(
            player,
            Vec3 {
                x: 0.0,
                y: -0.2,
                z: 0.0,
            },
            &floor,
        );
        assert!((result.movement.y + 0.1).abs() < 1.0e-12);
        assert!(result.vertical_collision);
        assert!(result.on_ground);
    }

    #[test]
    fn wall_clips_horizontal_movement() {
        let player = Aabb::new(0.2, 0.0, 0.2, 0.8, 1.8, 0.8);
        let wall = [Aabb::new(1.0, 0.0, 0.0, 2.0, 2.0, 1.0)];
        let result = collide(
            player,
            Vec3 {
                x: 0.5,
                y: 0.0,
                z: 0.0,
            },
            &wall,
        );
        assert!((result.movement.x - 0.2).abs() < 1.0e-12);
        assert!(result.horizontal_collision);
    }
}
