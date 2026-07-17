//! Vanilla-aligned player AABB collision primitives.

use std::sync::OnceLock;

/// Vanilla `Mth` sine lookup table: 65536 entries over a full turn. The client
/// and server both drive movement through this table rather than `Math.sin`,
/// so reproducing it bit-for-bit is what lets a server's movement re-simulation
/// agree with ours for any non-cardinal facing.
fn sin_table() -> &'static [f32; 65536] {
    static TABLE: OnceLock<Box<[f32; 65536]>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = Box::new([0.0f32; 65536]);
        for (i, slot) in table.iter_mut().enumerate() {
            *slot = (i as f64 * std::f64::consts::PI * 2.0 / 65536.0).sin() as f32;
        }
        table
    })
}

/// `Mth.sin`: table index `(int)(radians * 10430.378F) & 65535`.
pub fn mc_sin(radians: f32) -> f32 {
    sin_table()[((radians * 10430.378_f32) as i32 & 0xffff) as usize]
}

/// `Mth.cos`: the sine table offset by a quarter turn (16384 entries).
pub fn mc_cos(radians: f32) -> f32 {
    sin_table()[((radians * 10430.378_f32 + 16384.0_f32) as i32 & 0xffff) as usize]
}

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

/// Clips a requested movement vector against the supplied world-space voxel
/// boxes, returning the actual displacement. Vanilla resolves Y first, then one
/// horizontal axis followed by the other; the longer requested horizontal
/// component is resolved first. This is vanilla's `collideBoundingBox`.
pub fn clip_movement(aabb: Aabb, movement: Vec3, boxes: &[Aabb]) -> Vec3 {
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

    Vec3 { x, y, z }
}

/// Builds a [`CollisionResult`] from an actual displacement and the requested
/// one. A step-up leaves `movement.y` positive against a negative request, so
/// the same `vertical_collision && requested.y < 0` rule keeps the entity
/// grounded exactly as vanilla's `setOnGroundWithMovement` does.
fn collision_result(movement: Vec3, requested: Vec3) -> CollisionResult {
    let vertical_collision = movement.y != requested.y;
    CollisionResult {
        movement,
        horizontal_collision: movement.x != requested.x || movement.z != requested.z,
        vertical_collision,
        on_ground: vertical_collision && requested.y < 0.0,
    }
}

/// Clips movement with no step-up assist (the plain collision case).
pub fn collide(aabb: Aabb, movement: Vec3, boxes: &[Aabb]) -> CollisionResult {
    collision_result(clip_movement(aabb, movement, boxes), movement)
}

/// Clips movement with vanilla's auto step-up (`Entity.collide`): when a
/// grounded entity's horizontal move is blocked, it retries lifted by up to
/// `step_height` (0.6 for a player) and keeps the variant that travels farther
/// horizontally, then settles back down. This is what lets a walking bot climb
/// slabs, paths and single steps without jumping.
pub fn collide_with_step(
    aabb: Aabb,
    movement: Vec3,
    step_height: f64,
    on_ground: bool,
    boxes: &[Aabb],
) -> CollisionResult {
    let base = clip_movement(aabb, movement, boxes);
    let blocked_horizontally = base.x != movement.x || base.z != movement.z;
    let vertical_below = base.y != movement.y && movement.y < 0.0;
    let can_step = on_ground || vertical_below;

    let mut chosen = base;
    if step_height > 0.0 && can_step && blocked_horizontally {
        // Probe 1: the whole move, lifted by the full step height.
        let mut stepped = clip_movement(
            aabb,
            Vec3 {
                x: movement.x,
                y: step_height,
                z: movement.z,
            },
            boxes,
        );
        // Probe 2: lift first (over the box expanded toward the move), then
        // move horizontally from there — reaches steps the combined probe skims.
        let lift = clip_movement(
            expand_towards(aabb, movement.x, 0.0, movement.z),
            Vec3 {
                x: 0.0,
                y: step_height,
                z: 0.0,
            },
            boxes,
        );
        if lift.y < step_height {
            let after_lift = vec_add(
                clip_movement(
                    aabb.moved(lift.x, lift.y, lift.z),
                    Vec3 {
                        x: movement.x,
                        y: 0.0,
                        z: movement.z,
                    },
                    boxes,
                ),
                lift,
            );
            if horizontal_sqr(after_lift) > horizontal_sqr(stepped) {
                stepped = after_lift;
            }
        }
        if horizontal_sqr(stepped) > horizontal_sqr(base) {
            // Settle back down onto the stepped-up surface.
            let settle = clip_movement(
                aabb.moved(stepped.x, stepped.y, stepped.z),
                Vec3 {
                    x: 0.0,
                    y: -stepped.y + movement.y,
                    z: 0.0,
                },
                boxes,
            );
            chosen = vec_add(stepped, settle);
        }
    }

    collision_result(chosen, movement)
}

fn vec_add(a: Vec3, b: Vec3) -> Vec3 {
    Vec3 {
        x: a.x + b.x,
        y: a.y + b.y,
        z: a.z + b.z,
    }
}

fn horizontal_sqr(v: Vec3) -> f64 {
    v.x * v.x + v.z * v.z
}

/// Grows an AABB in the direction of `(dx, dy, dz)` (vanilla `expandTowards`).
fn expand_towards(aabb: Aabb, dx: f64, dy: f64, dz: f64) -> Aabb {
    Aabb::new(
        aabb.min_x + dx.min(0.0),
        aabb.min_y + dy.min(0.0),
        aabb.min_z + dz.min(0.0),
        aabb.max_x + dx.max(0.0),
        aabb.max_y + dy.max(0.0),
        aabb.max_z + dz.max(0.0),
    )
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

/// The default block friction shared by nearly every block (stone, dirt, …).
pub const DEFAULT_FRICTION: f64 = 0.6;

/// Vanilla `Block.getFriction()` for a 1.21.4 global state id: the default
/// `0.6` except for the ice family, blue ice and slime, whose overrides are
/// generated into [`collision_data::FRICTION_OVERRIDES`].
pub fn block_friction(state_id: u32) -> f64 {
    for &(lo, hi, friction) in super::collision_data::FRICTION_OVERRIDES {
        if state_id >= lo && state_id <= hi {
            return f64::from(friction);
        }
    }
    DEFAULT_FRICTION
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
    fn block_friction_matches_vanilla_overrides() {
        // Overrides come from f32 literals, so compare within f32 granularity.
        let approx = |got: f64, want: f64| (got - want).abs() < 1.0e-6;
        assert_eq!(block_friction(0), DEFAULT_FRICTION); // air
        assert_eq!(block_friction(1), DEFAULT_FRICTION); // stone
        assert!(approx(block_friction(5949), 0.98)); // ice
        assert!(approx(block_friction(11243), 0.8)); // slime block
        assert!(approx(block_friction(11625), 0.98)); // packed ice
        assert!(approx(block_friction(13554), 0.98)); // frosted ice (mid-range)
        assert!(approx(block_friction(13954), 0.989)); // blue ice
    }

    #[test]
    fn mth_trig_matches_vanilla_table_and_cardinals() {
        // Cardinal yaws used by moveRelative: exact table entries.
        assert!((mc_sin(0.0)).abs() < 1.0e-6);
        assert!((mc_cos(0.0) - 1.0).abs() < 1.0e-6);
        let half_pi = std::f32::consts::FRAC_PI_2;
        assert!((mc_sin(half_pi) - 1.0).abs() < 1.0e-4);
        assert!((mc_cos(half_pi)).abs() < 1.0e-4);
        // The table approximation stays within its ~1e-4 granularity of the
        // true trig functions across arbitrary angles.
        for step in -8..=8 {
            let r = step as f32 * 0.37;
            assert!((mc_sin(r) - r.sin()).abs() < 1.0e-3, "sin at {r}");
            assert!((mc_cos(r) - r.cos()).abs() < 1.0e-3, "cos at {r}");
        }
    }

    #[test]
    fn step_up_climbs_a_half_block_step() {
        // Feet at y=1 on a floor; a 0.5-high slab at x=1 blocks the path.
        let player = Aabb::new(0.2, 1.0, 0.2, 0.8, 2.8, 0.8);
        let boxes = [
            Aabb::new(0.0, 0.0, 0.0, 1.0, 1.0, 1.0), // floor
            Aabb::new(1.0, 1.0, 0.0, 2.0, 1.5, 1.0), // 0.5 slab step
        ];
        let result = collide_with_step(
            player,
            Vec3 {
                x: 0.4,
                y: -0.08,
                z: 0.0,
            },
            0.6,
            true,
            &boxes,
        );
        // Advanced in x and rose onto the slab top (feet 1.0 -> 1.5).
        assert!(
            result.movement.x > 0.39,
            "advanced past the step: {:?}",
            result.movement
        );
        assert!(
            (result.movement.y - 0.5).abs() < 1.0e-9,
            "stepped up 0.5: {:?}",
            result.movement
        );
        assert!(result.on_ground);
    }

    #[test]
    fn step_up_does_not_climb_a_full_block() {
        // A 1.0-high block exceeds the 0.6 step height, so the move is blocked.
        let player = Aabb::new(0.2, 1.0, 0.2, 0.8, 2.8, 0.8);
        let boxes = [
            Aabb::new(0.0, 0.0, 0.0, 1.0, 1.0, 1.0), // floor
            Aabb::new(1.0, 1.0, 0.0, 2.0, 2.0, 1.0), // full block wall
        ];
        let result = collide_with_step(
            player,
            Vec3 {
                x: 0.4,
                y: -0.08,
                z: 0.0,
            },
            0.6,
            true,
            &boxes,
        );
        assert!(
            result.movement.x.abs() < 0.21,
            "blocked by full block: {:?}",
            result.movement
        );
        assert!(result.horizontal_collision);
    }

    #[test]
    fn step_up_is_skipped_when_airborne() {
        // Not grounded and not landing: no step assist, the slab blocks the move.
        let player = Aabb::new(0.2, 1.0, 0.2, 0.8, 2.8, 0.8);
        let boxes = [Aabb::new(1.0, 1.0, 0.0, 2.0, 1.5, 1.0)];
        let result = collide_with_step(
            player,
            Vec3 {
                x: 0.4,
                y: 0.1, // rising, so `can_step` is false
                z: 0.0,
            },
            0.6,
            false,
            &boxes,
        );
        assert!(result.movement.x.abs() < 0.21, "no step while airborne");
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
