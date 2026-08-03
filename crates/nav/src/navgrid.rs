//! A navigation graph generated from the map itself.
//!
//! There is no `.graph` waypoint file for an arbitrary server's map rotation,
//! and hand-waypointing every map is not an option, so the grid is derived from
//! the only thing that is always present: the BSP's collision hulls.
//!
//! **The one rule this module lives by** is that no edge exists unless the
//! engine's own hull says the move is legal. Every candidate step is admitted
//! by a [`Bsp::hull_trace`] against hull 1 or hull 3 — the same pre-expanded
//! trees `PM_PlayerTrace` uses — with the same `sv_stepsize` (18,
//! `rehlds/engine/sv_phys.cpp:51`) and `mp_jump_height` (45,
//! `regamedll/dlls/game.cpp:176`) the server enforces. A guessed edge is worse
//! than a missing one: a missing edge makes the bot take the long way round, a
//! guessed one walks it into a wall.
//!
//! Shape of the thing:
//!
//! 1. **Ground-snap** — [`ground_snap`] drops a standing hull onto the floor.
//! 2. **Seed** from the spawns and objective volumes ([`MapInfo::seeds`]).
//! 3. **Flood fill** an axis-aligned lattice of [`CELL`] units, classifying
//!    every step with [`classify`].
//! 4. **Prune** isolated nodes and anything no spawn can reach.
//! 5. **Annotate** with the flags the router keys off.
//! 6. **Serialize** against a checksum of the `.bsp`, so a cache built for a
//!    different version of a map is rejected rather than trusted.

use std::collections::{HashMap, VecDeque};

use crate::bsp::{Bsp, Hull, Trace, Vec3};
use crate::entities::{Aabb, MapInfo, SolidBrush};
use crate::route::{self, NavSource};

/// Node flags.
///
/// Deliberately the same bits as [`crate::graph::flags`], so a router cannot
/// tell a generated grid from a loaded `.graph`.
pub mod flags {
    pub use crate::graph::flags::*;

    /// A local extension: YaPB has no buy-zone bit. Bit 11 is free there —
    /// `NARROW` is bit 10 and the next one used is `SNIPER` at bit 28.
    pub const BUY_ZONE: u32 = 1 << 11;
    /// Also local: this node is one of a team's spawn positions.
    pub const SPAWN: u32 = 1 << 12;
}

// ------------------------------------------------------------------ tuning

/// Horizontal lattice pitch. A player is 32 wide, so 40 leaves a node roughly
/// every stride without putting two nodes inside the same doorway.
pub const CELL: f32 = 40.0;

/// Two ground heights in the same column closer than this are the same place.
/// A standing player is 72 tall, so two genuinely stacked floors are always
/// further apart than this and stay distinct.
pub const Z_MERGE: f32 = 32.0;

/// `sv_stepsize` (`rehlds/engine/sv_phys.cpp:51`).
pub const STEP_SIZE: f32 = 18.0;

/// `mp_jump_height` is 45 (`regamedll/dlls/game.cpp:176`); one unit of margin
/// keeps a step that only just clears from being classified as reachable.
pub const MAX_JUMP: f32 = 44.0;

/// How far a bot is allowed to drop in one edge.
pub const MAX_FALL: f32 = 200.0;

/// The longest drop that costs no health.
///
/// `MAX_PLAYER_SAFE_FALL_SPEED` is 500 (`regamedll/dlls/player.h:62`) and
/// `sv_gravity` is 800 (`rehlds/engine/sv_phys.cpp:49`), so the free drop is
/// `500^2 / (2 * 800)` = 156.25 units. [`MAX_FALL`] is deliberately above it:
/// a bot that refuses every drop over 156 units cannot get down from a lot of
/// real geometry, and the 200-unit worst case costs about 11 health
/// (`DAMAGE_FOR_FALL_SPEED`, `player.h:73`). Lower `MAX_FALL` to this value if
/// you would rather the bots never took a scratch.
pub const SAFE_FALL: f32 = 156.25;

/// The steepest grade a player can run up without sliding back.
///
/// `PM_CatagorizePosition` keeps the player on ground while the surface normal
/// satisfies `normal[2] >= 0.7` (`pm_shared.cpp:1220`). Turned into a rise over
/// run that is `sqrt(1 - 0.7^2) / 0.7`, a little over 45 degrees. A slope
/// inside this limit is a walk, however much height it gains; a slope outside
/// it is not walkable at all, whatever the height.
pub const MAX_WALK_GRADE: f32 = 1.020_20;

/// Vertical spacing of the nodes on a ladder.
pub const LADDER_STEP: f32 = 32.0;

// ---------------------------------------------------- node radius (wayzones)

/// The scan distances the radius sweep tries, in order.
///
/// YaPB's loop is `for (scanDistance = 32; scanDistance < 128; scanDistance +=
/// 16)` (`yapb/src/graph.cpp:1470`), so the last distance actually tried is 112
/// and the largest radius that can survive the two `-= 16` steps is 96.
pub const RADIUS_SCANS: [f32; 6] = [32.0, 48.0, 64.0, 80.0, 96.0, 112.0];

/// What one failed probe costs, and the quantum every radius is a multiple of.
pub const RADIUS_STEP: f32 = 16.0;

/// How many directions each scan distance is probed in.
///
/// 18 at 20 degrees is a full turn. **YaPB's own loop does not do this**: it
/// ends each iteration with `direction.y = wrapAngle(direction.y +
/// circleRadius)` (`graph.cpp:1541`), adding the *loop counter* rather than the
/// step, so its yaw runs 0, 0, 20, 60, 120, 200, 300, 60, ... — some bearings
/// probed twice and others never. That is a typo for `+ 20.0f`, in the same
/// family as the two YaPB bugs the humanisation plan already refuses to port,
/// so this walks the circle evenly instead.
pub const RADIUS_DIRS: usize = 18;

/// How far below a probe point the floor is allowed to be.
///
/// The trace is `scan + 60` long (`graph.cpp:1508`): a node keeps its radius
/// only while the ground stays under the whole disc, so a bot jittered toward
/// the edge cannot be jittered off a ledge.
pub const RADIUS_DROP: f32 = 60.0;

/// Head clearance demanded at the edge of the disc (`graph.cpp:1531`).
pub const RADIUS_HEADROOM: f32 = 34.0;

/// The largest radius the sweep can return.
pub const MAX_RADIUS: f32 = 96.0;

/// The hull the sweep probes with.
///
/// YaPB passes `head_hull`, which is hull 3 — the ducking box
/// (`yapb/src/graph.cpp:1495`). Centred on a *standing* origin it spans the
/// player's waist to shoulders, which is the part of the body that actually
/// clips a corner when the bot cuts one.
const RADIUS_HULL: Hull = Hull::Duck;

/// Node classes that are never given a radius.
///
/// A bot must arrive *precisely* at these, so there is nothing to vary: YaPB
/// zeroes `Ladder | Goal | Camp | Rescue | Crouch` outright
/// (`yapb/src/graph.cpp:1456`). [`flags::CROUCH`] and [`flags::CAMP`] are never
/// set by generation today; they are listed because a `.graph` loaded through
/// the same flag set does set them, and because [`NavGrid::annotate`] sets
/// [`flags::NARROW`] on the nodes that are our crouch equivalent.
const NO_RADIUS: u32 = flags::LADDER
    | flags::GOAL
    | flags::CAMP
    | flags::RESCUE
    | flags::CROUCH
    | flags::NARROW;

// ------------------------------------------------------------- path smoothing

/// The longest hop [`NavGrid::smooth_path`] will merge a run of nodes into
/// (`yapb/src/planner.cpp:203`).
pub const SKIP_MAX_DIST: f32 = 400.0;

/// Two nodes further apart than this in z are not the same floor, so the
/// straight line between them is not walkable (`yapb/src/planner.cpp:191`).
pub const SKIP_MAX_RISE: f32 = 17.0;

/// Hard ceiling on the number of nodes.
///
/// A real map is bounded by solid space and by `models[0]`'s box, so this never
/// fires on one. It exists because generation is a flood fill, and a flood fill
/// on a map whose hull tree says "open" everywhere would otherwise not stop.
pub const MAX_NODES: usize = 200_000;

/// The eight lattice directions, in a fixed order so generation is repeatable.
const DIRS: [(i32, i32); 8] = [
    (1, 0),
    (1, 1),
    (0, 1),
    (-1, 1),
    (-1, 0),
    (-1, -1),
    (0, -1),
    (1, -1),
];

/// Where to look for a lattice-aligned seed: the cell itself, then its ring.
const SEED_RING: [(i32, i32); 9] = [
    (0, 0),
    (1, 0),
    (0, 1),
    (-1, 0),
    (0, -1),
    (1, 1),
    (1, -1),
    (-1, 1),
    (-1, -1),
];

// ------------------------------------------------------------------- model

/// How a bot gets from one node to the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Move {
    /// Flat ground or a step within `sv_stepsize`.
    Walk,
    /// Up to `mp_jump_height`, with head clearance.
    Jump,
    /// A drop. One-way: you cannot fall upwards.
    Fall,
    /// Standing does not fit but ducking does.
    Crouch,
    /// Along a `func_ladder`.
    Ladder,
    /// Through a `func_breakable`: legal, but the bot has to shoot it first.
    ///
    /// This exists because the alternative answers are both wrong. A breakable
    /// is the only way out of the CT spawn on de_prodigy, so calling it a wall
    /// strands the team; calling it open sends bots walking into crates on
    /// every other map. It is a passage that costs a magazine, and the router
    /// prices it that way.
    Break,
}

impl Move {
    /// Cost multiplier on the straight-line distance.
    ///
    /// Every one of these is `>= 1.0` on purpose: the router's heuristic is
    /// straight-line distance, and a multiplier below 1 would make it
    /// inadmissible and the "shortest" path wrong.
    pub fn cost_multiplier(self) -> f32 {
        match self {
            Self::Walk => 1.0,
            Self::Fall => 1.2,
            Self::Jump => 1.5,
            Self::Crouch => 2.0,
            Self::Ladder => 2.5,
            Self::Break => 8.0,
        }
    }

    fn to_byte(self) -> u8 {
        match self {
            Self::Walk => 0,
            Self::Jump => 1,
            Self::Fall => 2,
            Self::Crouch => 3,
            Self::Ladder => 4,
            Self::Break => 5,
        }
    }

    fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            0 => Self::Walk,
            1 => Self::Jump,
            2 => Self::Fall,
            3 => Self::Crouch,
            4 => Self::Ladder,
            5 => Self::Break,
            _ => return None,
        })
    }
}

/// One directed edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Link {
    pub to: u32,
    pub kind: Move,
}

/// One position a bot can stand (or, on a ladder, hang).
#[derive(Debug, Clone, PartialEq)]
pub struct NavNode {
    /// The player **origin**, not the feet: 36 above the floor it rests on.
    pub origin: Vec3,
    pub flags: u32,
    /// How far from [`origin`](Self::origin) the node still *is* this node.
    ///
    /// One of `{0, 16, 32, 48, 64, 80, 96}`, measured offline by
    /// [`node_radius`]. Zero means "stand exactly here" — a ladder, a bomb
    /// site, a doorway.
    ///
    /// This is the number that stops a route being a queue. A waypoint with a
    /// radius is a **disc**, not a point: bots steer at different spots inside
    /// it and count it as reached at different distances, so thirty bots
    /// following one corridor occupy its width instead of its centre line.
    pub radius: f32,
    pub links: Vec<Link>,
}

/// The generated graph.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NavGrid {
    pub nodes: Vec<NavNode>,
    /// Checksum of the `.bsp` this was generated from. See [`checksum`].
    pub checksum: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavError {
    TooShort,
    BadMagic(u32),
    BadVersion(u32),
    /// The cache was built from a different `.bsp`.
    StaleCache { expected: u64, found: u64 },
    /// A link points at a node that does not exist.
    LinkOutOfRange { node: usize, to: u32 },
    BadMoveKind(u8),
}

impl std::fmt::Display for NavError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort => write!(f, "nav cache truncated"),
            Self::BadMagic(m) => write!(f, "bad nav cache magic {m:#010x}"),
            Self::BadVersion(v) => write!(f, "unsupported nav cache version {v}"),
            Self::StaleCache { expected, found } => write!(
                f,
                "nav cache is for a different map: expected checksum {expected:#018x}, found {found:#018x}"
            ),
            Self::LinkOutOfRange { node, to } => {
                write!(f, "node {node} links to {to}, which does not exist")
            }
            Self::BadMoveKind(b) => write!(f, "unknown move kind {b}"),
        }
    }
}

impl std::error::Error for NavError {}

// -------------------------------------------------------------- primitives

fn add(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn raise(p: Vec3, dz: f32) -> Vec3 {
    [p[0], p[1], p[2] + dz]
}

/// FNV-1a over the whole `.bsp`.
///
/// Only used to tell "this cache was built from this file" from "it was not".
/// Two maps sharing a name and differing by one brush must not share a cache,
/// and that is all this has to guarantee.
pub fn checksum(bsp_bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bsp_bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The collision a player actually meets: the world hull **and** every solid
/// brush entity.
///
/// The world's clipnode trees hold worldspawn's brushes and nothing else. Every
/// crate, desk and pillar built as a `func_wall` or `func_breakable` is a
/// separate submodel with its own hull tree, and a trace against the world
/// passes straight through all of them — cs_office has 175. Getting this wrong
/// does not produce a subtly worse graph, it produces bots that walk through
/// furniture.
pub struct World<'a> {
    bsp: &'a Bsp,
    /// Hard blockers first, then the breakables. Ordering the vector this way
    /// means "everything" and "everything permanent" are both a slice, with no
    /// second allocation and no per-trace filtering.
    blockers: Vec<SolidBrush>,
    hard: usize,
    /// Union of every blocker's box, so a segment nowhere near one is rejected
    /// by a single test instead of a loop.
    span: Option<Aabb>,
}

fn expand_for(b: &Aabb, hull: Hull) -> Aabb {
    // Minkowski expansion, as `PM_TestPlayerPosition` does it
    // (`rehlds/engine/pmovetst.cpp:303-308`): the box a *point* must avoid.
    let (hmins, hmaxs) = (hull.mins(), hull.maxs());
    Aabb::new(
        [
            b.mins[0] - hmaxs[0],
            b.mins[1] - hmaxs[1],
            b.mins[2] - hmaxs[2],
        ],
        [
            b.maxs[0] - hmins[0],
            b.maxs[1] - hmins[1],
            b.maxs[2] - hmins[2],
        ],
    )
}

fn segment_box(a: Vec3, b: Vec3) -> Aabb {
    Aabb::new(
        [a[0].min(b[0]), a[1].min(b[1]), a[2].min(b[2])],
        [a[0].max(b[0]), a[1].max(b[1]), a[2].max(b[2])],
    )
}

impl<'a> World<'a> {
    /// The world hull only. Hand-built test maps have no brush entities, and
    /// nor does a caller who only wants line of sight.
    pub fn bare(bsp: &'a Bsp) -> Self {
        Self { bsp, blockers: Vec::new(), hard: 0, span: None }
    }

    pub fn new(bsp: &'a Bsp, info: &MapInfo) -> Self {
        let mut blockers: Vec<SolidBrush> = info
            .solid_brushes
            .iter()
            .filter(|b| b.model < bsp.models.len())
            .copied()
            .collect();
        blockers.sort_by_key(|b| b.breakable);
        let hard = blockers.iter().filter(|b| !b.breakable).count();
        let span = blockers.iter().map(|b| b.bounds).reduce(|a, b| {
            Aabb::new(
                [
                    a.mins[0].min(b.mins[0]),
                    a.mins[1].min(b.mins[1]),
                    a.mins[2].min(b.mins[2]),
                ],
                [
                    a.maxs[0].max(b.maxs[0]),
                    a.maxs[1].max(b.maxs[1]),
                    a.maxs[2].max(b.maxs[2]),
                ],
            )
        });
        Self { bsp, blockers, hard, span }
    }

    /// Does the map have anything a bot could shoot its way through?
    pub fn has_breakables(&self) -> bool {
        self.hard < self.blockers.len()
    }

    fn blockers_for(&self, include_breakable: bool) -> &[SolidBrush] {
        if include_breakable {
            &self.blockers
        } else {
            &self.blockers[..self.hard]
        }
    }

    pub fn bsp(&self) -> &Bsp {
        self.bsp
    }

    pub fn blockers(&self) -> &[SolidBrush] {
        &self.blockers
    }

    /// Nearest hit across the world and every overlapping brush entity.
    pub fn trace(&self, hull: Hull, a: Vec3, b: Vec3) -> Trace {
        self.trace_with(hull, a, b, true)
    }

    /// [`World::trace`] pretending every `func_breakable` has already been
    /// shot away.
    pub fn trace_ignoring_breakables(&self, hull: Hull, a: Vec3, b: Vec3) -> Trace {
        self.trace_with(hull, a, b, false)
    }

    fn trace_with(&self, hull: Hull, a: Vec3, b: Vec3, breakables: bool) -> Trace {
        let mut best = self.bsp.hull_trace(hull, a, b);
        if best.start_solid {
            return best;
        }
        let Some(span) = self.span else {
            return best;
        };
        let seg = segment_box(a, b);
        if !seg.intersects(&expand_for(&span, hull)) {
            return best;
        }
        for br in self.blockers_for(breakables) {
            if !seg.intersects(&expand_for(&br.bounds, hull)) {
                continue;
            }
            let t = self.bsp.hull_trace_model(br.model, hull, a, b);
            if t.start_solid {
                // Same convention as a single tree: a trace that begins inside
                // solid reports it and leaves the fraction alone.
                best.start_solid = true;
                best.fraction = 1.0;
                return best;
            }
            if t.fraction < best.fraction {
                best = t;
            }
        }
        best
    }

    pub fn clear(&self, hull: Hull, a: Vec3, b: Vec3) -> bool {
        self.trace_with(hull, a, b, true).is_clear()
    }

    fn clear_with(&self, hull: Hull, a: Vec3, b: Vec3, breakables: bool) -> bool {
        self.trace_with(hull, a, b, breakables).is_clear()
    }

    /// Can a player of this hull have its origin at `p`, once anything
    /// breakable in the way has been shot?
    ///
    /// Breakables are deliberately not consulted. They are obstacles a player
    /// removes, not geometry: the floor behind a breakable window is somewhere
    /// a bot really can stand, and refusing to put a node there is what strands
    /// the CT team on de_prodigy. What a breakable does affect is the *edges* —
    /// see [`classify`], which prices them as [`Move::Break`].
    pub fn fits(&self, hull: Hull, p: Vec3) -> bool {
        if self.bsp.hull_point_contents(hull, p) == crate::bsp::contents::SOLID {
            return false;
        }
        for br in self.blockers_for(false) {
            if !expand_for(&br.bounds, hull).contains(p) {
                continue;
            }
            if self.bsp.hull_point_contents_model(br.model, hull, p)
                == crate::bsp::contents::SOLID
            {
                return false;
            }
        }
        true
    }

    /// Is there a continuous surface under the straight line from `a` to `b`?
    ///
    /// A clear diagonal on its own is not a ramp — it is also what you get by
    /// drawing a line through thin air from a floor to a ledge. Probing the
    /// midpoint and requiring the ground there to track the line is what tells
    /// a staircase from a gap: on stairs the floor is right where the line is,
    /// over a gap it is a storey below.
    pub fn has_ground_between(&self, a: Vec3, b: Vec3) -> bool {
        let mid = [
            (a[0] + b[0]) * 0.5,
            (a[1] + b[1]) * 0.5,
            (a[2] + b[2]) * 0.5,
        ];
        match self.drop_to_floor(mid[0], mid[1], mid[2] + STEP_SIZE, STEP_SIZE * 2.0) {
            Some(g) => (g[2] - mid[2]).abs() <= STEP_SIZE,
            None => false,
        }
    }

    /// Where the floor is, ignoring breakables for the same reason
    /// [`World::fits`] does.
    fn drop_to_floor(&self, x: f32, y: f32, from_z: f32, distance: f32) -> Option<Vec3> {
        let t = self.trace_ignoring_breakables(
            Hull::Stand,
            [x, y, from_z],
            [x, y, from_z - distance],
        );
        if t.start_solid || t.fraction >= 1.0 {
            return None;
        }
        // A face steeper than about 45 degrees is not ground: the engine takes
        // the player off it and they slide (`pm_shared.cpp:1220`). A node there
        // would be a place the bot cannot actually stay.
        if !t.is_walkable_floor() {
            return None;
        }
        Some(t.end)
    }

    /// The highest floor in the column `(x, y)` between `from + MAX_JUMP` and
    /// `from - MAX_FALL` — the whole band one move can reach.
    ///
    /// Three starts, not one. Lifting the probe by a jump's worth is what finds
    /// a ledge you could jump onto, but under a low ceiling that lift begins
    /// *inside* solid, and "the probe started in a wall" is not the same fact as
    /// "there is no floor here". Dropping back to a step's worth and then to
    /// level with the source recovers those columns. de_dust2 has a node under
    /// exactly such a ceiling, which is how this was found.
    pub fn floor_in_window(&self, x: f32, y: f32, from: f32) -> Option<Vec3> {
        for lift in [MAX_JUMP, STEP_SIZE, 0.0] {
            if let Some(g) = self.drop_to_floor(x, y, from + lift, lift + MAX_FALL) {
                return Some(g);
            }
        }
        None
    }
}

/// Drop a standing player onto the floor under `p`.
///
/// Probes from `p + (0,0,18)` — one step's worth of slack, so a position a
/// little inside the floor still snaps — down to `p - (0,0,200)`. Returns the
/// legal standing **origin**, or `None` if the start is already inside solid,
/// there is no floor within reach, or what it found is too steep to stand on.
pub fn ground_snap(world: &World, p: Vec3) -> Option<Vec3> {
    world.drop_to_floor(p[0], p[1], p[2] + STEP_SIZE, STEP_SIZE + MAX_FALL)
}

/// A horizontal move with the engine's step-up.
///
/// `PM_StepUp` does not give up when the direct move is blocked: it lifts the
/// player by `sv_stepsize`, moves, and drops back down (`pm_shared.cpp:1196`,
/// `:1214`). Without modelling that, every kerb in the map would read as a
/// wall, because the expanded hull turns a 4-unit step into a 4-unit cliff face
/// sitting 16 units out from the real one.
fn step_move(world: &World, hull: Hull, a: Vec3, b: Vec3, breakables: bool) -> bool {
    if world.clear_with(hull, a, b, breakables) {
        return true;
    }
    let ah = raise(a, STEP_SIZE);
    let bh = raise(b, STEP_SIZE);
    world.clear_with(hull, a, ah, breakables)
        && world.clear_with(hull, ah, bh, breakables)
        && world.clear_with(hull, bh, b, breakables)
}

/// Can a player get from origin `from` to origin `to` in one move, and how?
///
/// **This function is the definition of an edge.** Generation calls it to
/// decide what to record, and the tests call it again on every consecutive pair
/// of a returned path — so "the router produced this path" and "the engine
/// permits this path" are checked against the same predicate rather than two
/// that merely resemble each other.
///
/// Both arguments are player origins. `None` means no legal move.
pub fn classify(world: &World, from: Vec3, to: Vec3) -> Option<Move> {
    if let Some(m) = classify_with(world, from, to, true) {
        return Some(m);
    }
    // Nothing in the way but a breakable? Then the move is legal, at the price
    // of shooting it. Anything that fails even with the breakables gone is a
    // wall and stays a wall.
    if world.has_breakables() && classify_with(world, from, to, false).is_some() {
        return Some(Move::Break);
    }
    None
}

fn classify_with(world: &World, from: Vec3, to: Vec3, breakables: bool) -> Option<Move> {
    let dz = to[2] - from[2];
    let horizontal = {
        let (dx, dy) = (to[0] - from[0], to[1] - from[1]);
        (dx * dx + dy * dy).sqrt()
    };

    // A ramp or staircase you simply run up or down. The lattice pitch is 40
    // units, so a flight of stairs gains far more than `sv_stepsize` between
    // two adjacent nodes even though a player walks it without pressing jump --
    // which is why the step-size test alone leaves the upper and lower floors
    // of a map like cs_747 as separate islands.
    //
    // The admission rule is the engine's own walkable-slope limit: the direct
    // hull trace has to be clear *and* the grade has to be one the player would
    // not slide back down (`WALKABLE_NORMAL_Z`, `pm_shared.cpp:1220`).
    if dz.abs() > STEP_SIZE
        && horizontal > 0.0
        && dz.abs() <= horizontal * MAX_WALK_GRADE
        && world.has_ground_between(from, to)
        && world.clear_with(Hull::Stand, from, to, breakables)
    {
        return Some(Move::Walk);
    }

    if dz.abs() <= STEP_SIZE {
        if step_move(world, Hull::Stand, from, to, breakables) {
            return Some(Move::Walk);
        }
        // Standing does not fit. A ducking origin sits 18 above the feet
        // instead of 36, so drop both ends by 18 to keep the feet where they
        // were and ask hull 3 the same question.
        let d = Hull::Stand.eye_to_feet() - Hull::Duck.eye_to_feet();
        if step_move(world, Hull::Duck, raise(from, -d), raise(to, -d), breakables) {
            return Some(Move::Crouch);
        }
        return None;
    }

    if dz > STEP_SIZE && dz <= MAX_JUMP {
        // Rise straight up first -- that vertical trace is the head-clearance
        // check -- and only then move across.
        let apex = [from[0], from[1], to[2]];
        if world.clear_with(Hull::Stand, from, apex, breakables)
            && world.clear_with(Hull::Stand, apex, to, breakables)
        {
            return Some(Move::Jump);
        }
        return None;
    }

    if (-MAX_FALL..-STEP_SIZE).contains(&dz) {
        // Walk off the ledge, then fall. If a railing blocks the first leg
        // there is nothing to fall from.
        let over = [to[0], to[1], from[2]];
        if world.clear_with(Hull::Stand, from, over, breakables)
            && world.clear_with(Hull::Stand, over, to, breakables)
        {
            return Some(Move::Fall);
        }
        return None;
    }

    None
}

// ------------------------------------------------------------- node radius

/// Would a hull of this size be inside solid at `p`?
///
/// This is YaPB's degenerate `testHull (start, start, ...)`
/// (`yapb/src/graph.cpp:1495`): a zero-length trace reports nothing but whether
/// the box fits where it began.
fn solid_at(world: &World, hull: Hull, p: Vec3) -> bool {
    world.trace(hull, p, p).start_solid
}

/// Is there ground within `reach` below `p`?
///
/// A trace that starts inside solid leaves the fraction at 1.0 (see
/// [`crate::bsp::Trace`]), so it answers "no floor" here — which is the
/// conservative answer and the one that shrinks the radius.
fn floor_within(world: &World, p: Vec3, reach: f32) -> bool {
    world.trace(RADIUS_HULL, p, raise(p, -reach)).fraction < 1.0
}

/// How much room a bot has around a node, computed once at grid-build time.
///
/// A port of `BotGraph::calculatePathRadius` (`yapb/src/graph.cpp:1451-1545`).
/// The sweep grows a disc outwards in 16-unit steps and stops at the first
/// direction that fails, so the answer is "the largest disc that is open all
/// the way round, floored, and with headroom" — with two 16-unit safety
/// margins subtracted, one for the failing step and one after the loop.
///
/// Each direction asks four questions at `origin + forward * scan`:
///
/// 1. does the hull fit out there at all;
/// 2. is there floor under it, within `scan + 60`;
/// 3. is there floor under the *opposite* side too — this is what keeps a node
///    on the lip of a drop from claiming the open air beyond it;
/// 4. is there 34 units of headroom above it.
///
/// The one thing not ported is YaPB's door check (`graph.cpp:1499-1505`, radius
/// 0 when the blocking entity is a door). `MapInfo` deliberately does not treat
/// `func_door` as solid at all (`entities.rs:309`) — a door opens — so there is
/// no door for a trace here to hit, and the frame around it is ordinary world
/// geometry that shrinks the radius on its own.
pub fn node_radius(world: &World, origin: Vec3) -> f32 {
    let mut radius = 0.0f32;
    'sweep: for &scan in &RADIUS_SCANS {
        radius = scan;
        for step in 0..RADIUS_DIRS {
            let yaw = step as f32 * (360.0 / RADIUS_DIRS as f32);
            let (sin, cos) = yaw.to_radians().sin_cos();
            let (dx, dy) = (cos * scan, sin * scan);
            let out = [origin[0] + dx, origin[1] + dy, origin[2]];
            let back = [origin[0] - dx, origin[1] - dy, origin[2]];

            let blocked = solid_at(world, RADIUS_HULL, out)
                || !floor_within(world, out, scan + RADIUS_DROP)
                || !floor_within(world, back, scan + RADIUS_DROP)
                || !world.clear(RADIUS_HULL, out, raise(out, RADIUS_HEADROOM));

            if blocked {
                radius -= RADIUS_STEP;
                break 'sweep;
            }
        }
    }
    (radius - RADIUS_STEP).max(0.0)
}

// ---------------------------------------------------------------- building

struct Builder<'a> {
    world: &'a World<'a>,
    nodes: Vec<NavNode>,
    /// `(lattice x, lattice y) -> node indices in that column, in the order
    /// they were created`. A `Vec` rather than a z bucket so that two probes of
    /// the same sloped floor cannot land either side of a bucket boundary and
    /// become two nodes a hair apart.
    columns: HashMap<(i32, i32), Vec<u32>>,
    /// Node is part of a ladder chain and is *not* standing on anything.
    airborne: Vec<bool>,
    /// `models[0]`'s bounding box. Nothing outside it is part of the map.
    bounds: Aabb,
}

fn lattice(p: Vec3) -> (i32, i32) {
    ((p[0] / CELL).round() as i32, (p[1] / CELL).round() as i32)
}

fn lattice_pos(ix: i32, iy: i32) -> (f32, f32) {
    (ix as f32 * CELL, iy as f32 * CELL)
}

impl<'a> Builder<'a> {
    fn new(world: &'a World<'a>) -> Self {
        let w = world.bsp().models[0];
        Self {
            world,
            nodes: Vec::new(),
            columns: HashMap::new(),
            airborne: Vec::new(),
            bounds: Aabb::new(w.mins, w.maxs),
        }
    }

    /// Is this lattice column inside the world at all?
    ///
    /// On a real map the answer never matters — everything past the edge of the
    /// map is `CONTENTS_SOLID` in the clip hulls, so the probe fails anyway.
    /// It matters for a hand-built map whose tree describes an unbounded plane,
    /// where without it the fill would walk outwards forever.
    fn in_world(&self, x: f32, y: f32) -> bool {
        x >= self.bounds.mins[0]
            && x <= self.bounds.maxs[0]
            && y >= self.bounds.mins[1]
            && y <= self.bounds.maxs[1]
    }

    /// Existing node in this column within [`Z_MERGE`] of `z`.
    fn find(&self, ix: i32, iy: i32, z: f32) -> Option<u32> {
        self.columns.get(&(ix, iy))?.iter().copied().find(|&i| {
            (self.nodes[i as usize].origin[2] - z).abs() <= Z_MERGE
        })
    }

    /// Insert, or reuse a node already at this spot. Flags are OR-ed in.
    fn insert(&mut self, ix: i32, iy: i32, origin: Vec3, flags: u32, airborne: bool) -> u32 {
        if let Some(i) = self.find(ix, iy, origin[2]) {
            self.nodes[i as usize].flags |= flags;
            // A ladder node that turned out to coincide with a floor node is
            // standing on the floor, not hanging.
            if !airborne {
                self.airborne[i as usize] = false;
            }
            return i;
        }
        let i = self.nodes.len() as u32;
        // The radius is measured once the graph is final -- see
        // [`NavGrid::measure_radii`]. Sweeping here would pay for every node
        // the prune is about to throw away.
        self.nodes.push(NavNode { origin, flags, radius: 0.0, links: Vec::new() });
        self.airborne.push(airborne);
        self.columns.entry((ix, iy)).or_default().push(i);
        i
    }

    fn link(&mut self, from: u32, to: u32, kind: Move) {
        if from == to {
            return;
        }
        let links = &mut self.nodes[from as usize].links;
        if links.iter().any(|l| l.to == to) {
            return;
        }
        links.push(Link { to, kind });
    }

    /// Ground-snap `p` onto the lattice and register it. Tries the containing
    /// cell first, then its ring, so a spawn tucked against a wall still gets a
    /// node instead of being dropped.
    fn seed(&mut self, p: Vec3, flags: u32) -> Option<u32> {
        let (ix, iy) = lattice(p);
        for (dx, dy) in SEED_RING {
            let (jx, jy) = (ix + dx, iy + dy);
            let (x, y) = lattice_pos(jx, jy);
            if let Some(g) = ground_snap(self.world, [x, y, p[2]]) {
                return Some(self.insert(jx, jy, g, flags, false));
            }
        }
        None
    }

    /// The nodes of one `func_ladder`, bottom to top.
    ///
    /// The brush is a thin slab flat against the wall, so its centre is inside
    /// the wall as far as the player hull is concerned. The node column is
    /// therefore placed on the nearest lattice point that a standing player
    /// actually fits in, within one cell of the ladder.
    fn ladder_chain(&mut self, zone: &Aabb) -> Vec<u32> {
        let c = zone.centre();
        let mut chain = Vec::new();

        // Pick the lattice column: nearest first, then the ring.
        let (ix, iy) = lattice(c);
        let mut column = None;
        for (dx, dy) in SEED_RING {
            let (jx, jy) = (ix + dx, iy + dy);
            let (x, y) = lattice_pos(jx, jy);
            // The column is usable if a standing player fits somewhere on it.
            let fits = self.probe_ladder_column(x, y, zone).is_some();
            if fits {
                column = Some((jx, jy, x, y));
                break;
            }
        }
        let Some((jx, jy, x, y)) = column else {
            return chain;
        };

        let feet_to_origin = Hull::Stand.eye_to_feet();
        let mut z = zone.mins[2] + feet_to_origin;
        let top = zone.maxs[2] + feet_to_origin;
        loop {
            let p = [x, y, z];
            if self.world.fits(Hull::Stand, p) {
                chain.push(self.insert(jx, jy, p, flags::LADDER, true));
            }
            if z >= top {
                break;
            }
            z = (z + LADDER_STEP).min(top);
        }

        // Vertical links, both ways, but only where the hull agrees you can
        // actually move between the two.
        for w in chain.windows(2) {
            let (a, b) = (w[0], w[1]);
            let (pa, pb) = (self.nodes[a as usize].origin, self.nodes[b as usize].origin);
            if self.world.clear(Hull::Stand, pa, pb) {
                self.link(a, b, Move::Ladder);
                self.link(b, a, Move::Ladder);
            }
        }
        chain
    }

    fn probe_ladder_column(&self, x: f32, y: f32, zone: &Aabb) -> Option<f32> {
        let feet_to_origin = Hull::Stand.eye_to_feet();
        let mut z = zone.mins[2] + feet_to_origin;
        let top = zone.maxs[2] + feet_to_origin;
        loop {
            if self.world.fits(Hull::Stand, [x, y, z]) {
                return Some(z);
            }
            if z >= top {
                return None;
            }
            z = (z + LADDER_STEP).min(top);
        }
    }

    /// Breadth-first expansion over the lattice.
    fn flood(&mut self, seeds: &[u32]) {
        let mut queue: VecDeque<u32> = seeds.iter().copied().collect();
        let mut expanded: Vec<bool> = vec![false; self.nodes.len()];

        while let Some(i) = queue.pop_front() {
            let iu = i as usize;
            if expanded.len() <= iu {
                expanded.resize(iu + 1, false);
            }
            if expanded[iu] {
                continue;
            }
            expanded[iu] = true;

            let p = self.nodes[iu].origin;
            let (ix, iy) = lattice(p);

            for (dx, dy) in DIRS {
                let (jx, jy) = (ix + dx, iy + dy);
                let (x, y) = lattice_pos(jx, jy);
                if !self.in_world(x, y) || self.nodes.len() >= MAX_NODES {
                    continue;
                }

                // Candidate 1: whatever floor is in that column inside the
                // reachable window.
                let found = self.world.floor_in_window(x, y, p[2]);
                let mut candidates: Vec<u32> = Vec::new();
                if let Some(g) = found {
                    let before = self.nodes.len();
                    let j = self.insert(jx, jy, g, 0, false);
                    if self.nodes.len() != before {
                        queue.push_back(j);
                    }
                    candidates.push(j);
                }
                // Candidate 2: nodes already in that column and inside the
                // window. This is what lets a bot step *onto* the top of a
                // ladder, which no ground probe would ever find.
                if let Some(existing) = self.columns.get(&(jx, jy)) {
                    for &j in existing {
                        let dz = self.nodes[j as usize].origin[2] - p[2];
                        if (-MAX_FALL..=MAX_JUMP).contains(&dz) && !candidates.contains(&j) {
                            candidates.push(j);
                        }
                    }
                }

                for j in candidates {
                    if j == i {
                        continue;
                    }
                    let q = self.nodes[j as usize].origin;
                    let Some(kind) = classify(self.world, p, q) else {
                        continue;
                    };
                    // A node hanging on a ladder is only reachable by stepping
                    // on or off it, never by jumping to or falling onto a point
                    // in mid-air.
                    if (self.airborne[iu] || self.airborne[j as usize])
                        && !matches!(kind, Move::Walk | Move::Crouch)
                    {
                        continue;
                    }
                    self.link(i, j, kind);
                }
            }
        }
    }
}

// -------------------------------------------------------------- generation

impl NavGrid {
    /// Generate a grid for a parsed map.
    pub fn generate(bsp: &Bsp, info: &MapInfo, checksum: u64) -> Self {
        let world = World::new(bsp, info);
        let mut b = Builder::new(&world);

        // Ladder columns first, so the flood fill links to them rather than
        // creating a second node on top of them.
        let mut seeds: Vec<u32> = Vec::new();
        for zone in &info.ladders {
            seeds.extend(b.ladder_chain(zone));
        }

        let mut spawn_nodes: Vec<u32> = Vec::new();
        for &p in info.t_spawns.iter().chain(&info.ct_spawns) {
            if let Some(i) = b.seed(p, flags::SPAWN) {
                spawn_nodes.push(i);
                seeds.push(i);
            }
        }
        for &p in &info.hostage_spawns {
            if let Some(i) = b.seed(p, 0) {
                seeds.push(i);
            }
        }
        for zone in info
            .bomb_sites
            .iter()
            .chain(&info.rescue_zones)
            .chain(&info.buy_zones)
        {
            if let Some(i) = b.seed(zone.centre(), 0) {
                seeds.push(i);
            }
        }

        b.flood(&seeds);

        let mut grid = Self { nodes: b.nodes, checksum };
        grid.annotate(info);

        // Reachability is measured from the spawns: those are the only places a
        // bot ever starts. An island the map has no route to is not navigation
        // data, it is noise that makes every A* run slower.
        let roots: Vec<usize> = spawn_nodes.iter().map(|&i| i as usize).collect();
        grid.prune(&roots);

        // Last, on the nodes that survived: the sweep is the most expensive
        // part of generation and there is no point measuring a node nobody can
        // reach.
        grid.measure_radii(&world);
        grid
    }

    /// Parse, derive and generate in one step.
    pub fn for_bsp_bytes(data: &[u8]) -> Result<(Bsp, MapInfo, Self), Box<dyn std::error::Error>> {
        let bsp = Bsp::parse(data)?;
        let info = MapInfo::from_bsp(&bsp)?;
        let grid = Self::generate(&bsp, &info, checksum(data));
        Ok((bsp, info, grid))
    }

    /// Tag nodes that stand inside an objective volume.
    ///
    /// The test is an AABB overlap between the player's box and the trigger's,
    /// which is exactly how the server decides you are in a buy zone or on a
    /// bomb site — not a centre-point-in-box test, which would miss a bot
    /// standing on the edge of a site the server considers it to be on.
    fn annotate(&mut self, info: &MapInfo) {
        for n in &mut self.nodes {
            let player = Aabb::new(
                add(n.origin, Hull::Stand.mins()),
                add(n.origin, Hull::Stand.maxs()),
            );
            if info.bomb_sites.iter().any(|z| player.intersects(z)) {
                n.flags |= flags::GOAL;
            }
            if info.rescue_zones.iter().any(|z| player.intersects(z)) {
                n.flags |= flags::RESCUE;
            }
            if info.buy_zones.iter().any(|z| player.intersects(z)) {
                n.flags |= flags::BUY_ZONE;
            }
            // Our stand-in for YaPB's hand-placed `NodeFlag::Crouch` /
            // `NodeFlag::Narrow`. A node you can only leave by ducking is the
            // mouth of a gap a player barely fits through, which is exactly
            // where a bot must not be handed a jittered target or allowed to
            // cut the corner.
            if n.links.iter().any(|l| l.kind == Move::Crouch) {
                n.flags |= flags::NARROW;
            }
        }
    }

    /// Measure every node's [`radius`](NavNode::radius).
    ///
    /// Two classes never get one, matching `graph.cpp:1456-1466`: the node
    /// types a bot has to hit precisely, and anything linked to a ladder —
    /// stepping off a ladder is a placement problem, and a bot aiming at a
    /// point 60 units from the rung misses it.
    fn measure_radii(&mut self, world: &World) {
        let is_ladder: Vec<bool> = self
            .nodes
            .iter()
            .map(|n| n.flags & flags::LADDER != 0)
            .collect();
        for i in 0..self.nodes.len() {
            let n = &self.nodes[i];
            let precise = n.flags & NO_RADIUS != 0
                || n.links.iter().any(|l| {
                    matches!(l.kind, Move::Ladder | Move::Crouch) || is_ladder[l.to as usize]
                });
            self.nodes[i].radius = if precise {
                0.0
            } else {
                node_radius(world, self.nodes[i].origin)
            };
        }
    }

    /// Drop nodes with no edges at all, then anything `roots` cannot reach.
    ///
    /// If `roots` is empty nothing is dropped for unreachability — a map with
    /// no spawns gives no evidence about what is reachable, and throwing the
    /// whole graph away on no evidence would be worse than keeping it.
    fn prune(&mut self, roots: &[usize]) {
        let mut keep: Vec<bool> = self
            .nodes
            .iter()
            .map(|n| !n.links.is_empty())
            .collect();
        // A node with no outgoing links is still worth keeping if something
        // links *to* it: it is a dead end you can walk into, such as a pit.
        for n in &self.nodes {
            for l in &n.links {
                keep[l.to as usize] = true;
            }
        }

        if !roots.is_empty() {
            let reachable = route::reachable_from(self, roots);
            for (i, k) in keep.iter_mut().enumerate() {
                *k &= reachable[i];
            }
        }

        if keep.iter().all(|&k| k) {
            return;
        }

        let mut remap = vec![u32::MAX; self.nodes.len()];
        let mut next = 0u32;
        for (i, &k) in keep.iter().enumerate() {
            if k {
                remap[i] = next;
                next += 1;
            }
        }
        let old = std::mem::take(&mut self.nodes);
        self.nodes = old
            .into_iter()
            .enumerate()
            .filter(|(i, _)| keep[*i])
            .map(|(_, mut n)| {
                n.links.retain(|l| remap[l.to as usize] != u32::MAX);
                for l in &mut n.links {
                    l.to = remap[l.to as usize];
                }
                n
            })
            .collect();
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn edge_count(&self) -> usize {
        self.nodes.iter().map(|n| n.links.len()).sum()
    }

    /// The node closest to a world position.
    pub fn nearest(&self, p: Vec3) -> Option<usize> {
        route::nearest(self, p)
    }

    /// Every node carrying `flag`.
    pub fn nodes_with_flag(&self, flag: u32) -> Vec<usize> {
        route::nodes_with_flag(self, flag)
    }

    /// A\* between two nodes. Shares its implementation with the `.graph`
    /// loader — see [`crate::route`].
    pub fn find_path(&self, start: usize, goal: usize) -> Option<Vec<usize>> {
        route::find_path(self, start, goal)
    }

    /// [`Self::find_path`], charging extra to enter nodes that have already
    /// defeated us. See [`route::find_path_avoiding`].
    pub fn find_path_avoiding(
        &self,
        start: usize,
        goal: usize,
        penalty: &dyn Fn(usize) -> f32,
    ) -> Option<Vec<usize>> {
        route::find_path_avoiding(self, start, goal, penalty)
    }

    /// [`Self::find_path_avoiding`] with the heuristic weight from
    /// [`route::find_path_tuned`] -- the knob that makes two bots with the same
    /// goal walk different routes.
    pub fn find_path_tuned(
        &self,
        start: usize,
        goal: usize,
        penalty: &dyn Fn(usize) -> f32,
        h_weight: f32,
    ) -> Option<Vec<usize>> {
        route::find_path_tuned(self, start, goal, penalty, h_weight)
    }

    /// How far from node `i` still counts as being at node `i`.
    pub fn radius(&self, i: usize) -> f32 {
        self.nodes.get(i).map_or(0.0, |n| n.radius)
    }

    // ----------------------------------------------------------- smoothing

    /// Where every node sits on the lattice, so a straight line can be walked
    /// cell by cell without a scan over the whole graph per step.
    fn columns(&self) -> HashMap<(i32, i32), Vec<u32>> {
        let mut cols: HashMap<(i32, i32), Vec<u32>> = HashMap::new();
        for (i, n) in self.nodes.iter().enumerate() {
            cols.entry(lattice(n.origin)).or_default().push(i as u32);
        }
        cols
    }

    /// The node in lattice cell `cell` closest in height to `z`.
    fn node_in(&self, cols: &HashMap<(i32, i32), Vec<u32>>, cell: (i32, i32), z: f32) -> Option<usize> {
        let best = cols.get(&cell)?.iter().copied().min_by(|&a, &b| {
            let (da, db) = (
                (self.nodes[a as usize].origin[2] - z).abs(),
                (self.nodes[b as usize].origin[2] - z).abs(),
            );
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })?;
        ((self.nodes[best as usize].origin[2] - z).abs() <= Z_MERGE).then_some(best as usize)
    }

    /// Is there a walkable edge `from -> to`, or are they the same node?
    fn walks_between(&self, from: usize, to: usize) -> bool {
        from == to || self.move_between(from, to) == Some(Move::Walk)
    }

    /// Can a bot walk the *straight line* between two nodes?
    ///
    /// YaPB answers this from `vistab`, a per-node-pair visibility bitmap built
    /// with traces when the graph is authored (`yapb/src/planner.cpp:186`). We
    /// cannot: the follower is handed a [`NavGrid`] and nothing else — no BSP,
    /// no `World`, no way to trace — and a 4715-node map would need eleven
    /// million traces and a 2.8 MB table to carry one.
    ///
    /// So this asks the graph instead, and the graph is not a weaker source
    /// than a trace: **every edge in it was admitted by a hull trace** through
    /// [`classify`]. Walk the lattice cells the line crosses; each one must
    /// hold a node at roughly the line's height, and consecutive ones must be
    /// joined by a [`Move::Walk`] edge. A wall between the two ends breaks that
    /// chain — either the cell inside it has no node, or the two cells either
    /// side of it have no edge, because `classify` refused to make one.
    ///
    /// Walk edges only, deliberately. A chain that needs a jump, a drop, a
    /// crouch or a breakable is a place a bot has to be steered *through*, not
    /// a corner it may cut.
    fn corridor_clear(&self, a: usize, b: usize, cols: &HashMap<(i32, i32), Vec<u32>>) -> bool {
        let (pa, pb) = (self.nodes[a].origin, self.nodes[b].origin);
        let span = route::dist(pa, pb);
        // Half a cell, so no sample can skip a cell the line passes through.
        let steps = ((span / (CELL * 0.5)).ceil() as usize).max(1);

        let mut prev = a;
        let mut prev_cell = lattice(pa);
        for s in 1..=steps {
            let t = s as f32 / steps as f32;
            let p = [
                pa[0] + (pb[0] - pa[0]) * t,
                pa[1] + (pb[1] - pa[1]) * t,
                pa[2] + (pb[2] - pa[2]) * t,
            ];
            let cell = lattice(p);
            if cell == prev_cell {
                continue;
            }
            let Some(next) = self.node_in(cols, cell, p[2]) else {
                return false;
            };
            if !self.walks_between(prev, next) {
                return false;
            }
            prev = next;
            prev_cell = cell;
        }
        self.walks_between(prev, b)
    }

    /// Must the route stop at a node between `a` and `b`?
    ///
    /// `AStarAlgo::cantSkipNode` (`yapb/src/planner.cpp:176-220`), minus one
    /// test. Its `tooClose` clause reads `distanceSq < cr::sqrtf (40.0f)` —
    /// `sqrtf`, not `sqrf`, so the threshold is 6.32 *square* units and the
    /// test fires only for two nodes less than 2.5 units apart. Verified
    /// against the source rather than assumed: `crlib`'s `sqrf` and `sqrtf` are
    /// both in scope there, and no graph puts two nodes that close. It is dead
    /// code, and reproducing it would only look like intent.
    pub fn cant_skip(
        &self,
        a: usize,
        b: usize,
        visible: &dyn Fn(usize, usize) -> bool,
    ) -> bool {
        let (na, nb) = (&self.nodes[a], &self.nodes[b]);
        // No radius means "be exactly here", and a node you must arrive at is
        // not one you may skip past.
        if na.radius <= 0.0 || nb.radius <= 0.0 {
            return true;
        }
        if (na.origin[2] - nb.origin[2]).abs() > SKIP_MAX_RISE {
            return true;
        }
        if (na.flags | nb.flags) & flags::NARROW != 0 {
            return true;
        }
        if route::dist(na.origin, nb.origin) > SKIP_MAX_DIST {
            return true;
        }
        // A jump is a button press at a place, not a direction of travel: merge
        // the node away and the bot walks into the lip it was meant to clear.
        if na.links.iter().chain(&nb.links).any(|l| l.kind == Move::Jump) {
            return true;
        }
        !visible(a, b)
    }

    /// Drop the nodes a bot does not need to visit.
    ///
    /// A\* on a 40-unit lattice returns a staircase: the shortest route across
    /// open ground is a zig-zag of 40-unit hops, and a bot that steers at every
    /// one of them walks the zig-zag. Greedy skip fixes exactly that — keep the
    /// last node emitted, and emit the next only when the one *after* it cannot
    /// be reached directly (`yapb/src/planner.cpp:222-240`).
    ///
    /// The result is never worse connected than the input: two consecutive
    /// nodes of the output are either adjacent in the input or a pair
    /// [`cant_skip`](Self::cant_skip) has already passed, so nothing further
    /// apart than [`SKIP_MAX_DIST`] survives.
    pub fn smooth_path(&self, path: &[usize]) -> Vec<usize> {
        let cols = self.columns();
        self.smooth_path_with(path, &|a, b| self.corridor_clear(a, b, &cols))
    }

    /// [`smooth_path`](Self::smooth_path) with the line-of-sight test supplied
    /// by the caller — for anyone holding a [`World`] and able to trace.
    pub fn smooth_path_with(
        &self,
        path: &[usize],
        visible: &dyn Fn(usize, usize) -> bool,
    ) -> Vec<usize> {
        if path.len() < 3 {
            return path.to_vec();
        }
        let mut out = vec![path[0]];
        for i in 1..path.len() - 1 {
            let last = *out.last().expect("seeded with path[0]");
            if self.cant_skip(last, path[i + 1], visible) {
                out.push(path[i]);
            }
        }
        out.push(path[path.len() - 1]);
        out
    }

    /// The move recorded for the edge `from -> to`, if there is one.
    pub fn move_between(&self, from: usize, to: usize) -> Option<Move> {
        self.nodes
            .get(from)?
            .links
            .iter()
            .find(|l| l.to as usize == to)
            .map(|l| l.kind)
    }

    // ------------------------------------------------------------ the cache

    /// `"NAVG"` little-endian.
    pub const MAGIC: u32 = 0x4756_414E;
    /// On-disk layout number.
    ///
    /// **Bump this whenever a node's serialized fields change.** Version 2
    /// added [`NavNode::radius`] between `flags` and the link count; a version
    /// 1 file read with the version 2 reader would take the first link's index
    /// as a radius and slide every field after it, producing a graph that loads
    /// without complaint and routes bots into walls. The version check is the
    /// only thing standing between a stale cache and that.
    pub const VERSION: u32 = 2;

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(24 + self.nodes.len() * 28);
        out.extend_from_slice(&Self::MAGIC.to_le_bytes());
        out.extend_from_slice(&Self::VERSION.to_le_bytes());
        out.extend_from_slice(&self.checksum.to_le_bytes());
        out.extend_from_slice(&(self.nodes.len() as u32).to_le_bytes());
        for n in &self.nodes {
            for v in n.origin {
                out.extend_from_slice(&v.to_le_bytes());
            }
            out.extend_from_slice(&n.flags.to_le_bytes());
            out.extend_from_slice(&n.radius.to_le_bytes());
            out.extend_from_slice(&(n.links.len() as u32).to_le_bytes());
            for l in &n.links {
                out.extend_from_slice(&l.to.to_le_bytes());
                out.push(l.kind.to_byte());
            }
        }
        out
    }

    /// Read a cache back, refusing one built from a different `.bsp`.
    pub fn from_bytes(data: &[u8], expected: u64) -> Result<Self, NavError> {
        let rd_u32 = |o: usize| -> Result<u32, NavError> {
            data.get(o..o + 4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .ok_or(NavError::TooShort)
        };
        let magic = rd_u32(0)?;
        if magic != Self::MAGIC {
            return Err(NavError::BadMagic(magic));
        }
        let version = rd_u32(4)?;
        if version != Self::VERSION {
            return Err(NavError::BadVersion(version));
        }
        let checksum = data
            .get(8..16)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
            .ok_or(NavError::TooShort)?;
        if checksum != expected {
            return Err(NavError::StaleCache { expected, found: checksum });
        }
        let count = rd_u32(16)? as usize;

        let mut o = 20;
        let mut nodes = Vec::with_capacity(count.min(1 << 20));
        for _ in 0..count {
            let mut origin = [0.0f32; 3];
            for v in &mut origin {
                *v = f32::from_bits(rd_u32(o)?);
                o += 4;
            }
            let flags = rd_u32(o)?;
            o += 4;
            let radius = f32::from_bits(rd_u32(o)?);
            o += 4;
            let nlinks = rd_u32(o)? as usize;
            o += 4;
            let mut links = Vec::with_capacity(nlinks.min(64));
            for _ in 0..nlinks {
                let to = rd_u32(o)?;
                o += 4;
                let kind = *data.get(o).ok_or(NavError::TooShort)?;
                o += 1;
                links.push(Link {
                    to,
                    kind: Move::from_byte(kind).ok_or(NavError::BadMoveKind(kind))?,
                });
            }
            nodes.push(NavNode { origin, flags, radius, links });
        }

        for (i, n) in nodes.iter().enumerate() {
            for l in &n.links {
                if l.to as usize >= nodes.len() {
                    return Err(NavError::LinkOutOfRange { node: i, to: l.to });
                }
            }
        }
        Ok(Self { nodes, checksum })
    }

    pub fn save(&self, path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
        std::fs::write(path, self.to_bytes())
    }

    pub fn load(path: impl AsRef<std::path::Path>, expected: u64) -> Result<Self, NavError> {
        let data = std::fs::read(path).map_err(|_| NavError::TooShort)?;
        Self::from_bytes(&data, expected)
    }
}

impl NavSource for NavGrid {
    fn len(&self) -> usize {
        self.nodes.len()
    }
    fn origin(&self, i: usize) -> Vec3 {
        self.nodes[i].origin
    }
    fn flags(&self, i: usize) -> u32 {
        self.nodes[i].flags
    }
    fn neighbours(&self, i: usize, out: &mut Vec<usize>) {
        out.extend(self.nodes[i].links.iter().map(|l| l.to as usize));
    }
    fn cost(&self, from: usize, to: usize) -> f32 {
        let d = route::dist(self.origin(from), self.origin(to));
        let m = self.nodes[from]
            .links
            .iter()
            .find(|l| l.to as usize == to)
            .map_or(1.0, |l| l.kind.cost_multiplier());
        d * m
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bsp::{contents, ClipNode, Leaf, Model, Node, Plane};
    use crate::entities::Scenario;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Instant;

    // ------------------------------------------------------ synthetic maps

    /// A flat floor at z = 0 inside a walled box, and nothing else.
    ///
    /// The walls sit at +-1000; the *expanded* walls are therefore at +-984,
    /// because both player hulls are 16 wide either side of the origin. Each
    /// hull gets its own tree, as the compiler would emit: hull 1's floor plane
    /// is at z = 36, hull 3's at z = 18.
    ///
    /// Walls are not decoration here. Without them the floor is unbounded and
    /// the flood fill has nothing to stop it.
    fn flat_map() -> Bsp {
        let wall = 1000.0 - 16.0;
        Bsp {
            planes: vec![
                Plane { normal: [0.0, 0.0, 1.0], dist: 36.0, kind: 2 },   // 0
                Plane { normal: [1.0, 0.0, 0.0], dist: wall, kind: 0 },   // 1
                Plane { normal: [1.0, 0.0, 0.0], dist: -wall, kind: 0 },  // 2
                Plane { normal: [0.0, 1.0, 0.0], dist: wall, kind: 1 },   // 3
                Plane { normal: [0.0, 1.0, 0.0], dist: -wall, kind: 1 },  // 4
                Plane { normal: [0.0, 0.0, 1.0], dist: 18.0, kind: 2 },   // 5
                Plane { normal: [0.0, 0.0, 1.0], dist: 0.0, kind: 2 },    // 6
            ],
            nodes: vec![Node { plane: 6, children: [-1, -2] }],
            leaves: vec![
                Leaf { contents: contents::EMPTY },
                Leaf { contents: contents::SOLID },
            ],
            clipnodes: vec![
                // hull 1, root 0
                ClipNode { plane: 0, children: [1, -2] },
                ClipNode { plane: 1, children: [-2, 2] },
                ClipNode { plane: 2, children: [3, -2] },
                ClipNode { plane: 3, children: [-2, 4] },
                ClipNode { plane: 4, children: [-1, -2] },
                // hull 3, root 5
                ClipNode { plane: 5, children: [6, -2] },
                ClipNode { plane: 1, children: [-2, 7] },
                ClipNode { plane: 2, children: [8, -2] },
                ClipNode { plane: 3, children: [-2, 9] },
                ClipNode { plane: 4, children: [-1, -2] },
            ],
            models: vec![Model {
                mins: [-1000.0, -1000.0, -64.0],
                maxs: [1000.0, 1000.0, 1000.0],
                origin: [0.0; 3],
                headnode: [0, 0, 0, 5],
            }],
            ..Default::default()
        }
    }

    #[test]
    fn the_synthetic_flat_map_is_actually_walled_in() {
        let m = flat_map();
        assert_eq!(m.hull_point_contents(Hull::Stand, [0.0, 0.0, 36.0]), contents::EMPTY);
        assert_eq!(m.hull_point_contents(Hull::Stand, [990.0, 0.0, 36.0]), contents::SOLID);
        assert_eq!(m.hull_point_contents(Hull::Stand, [0.0, -990.0, 36.0]), contents::SOLID);
        assert_eq!(m.hull_point_contents(Hull::Duck, [990.0, 0.0, 18.0]), contents::SOLID);
        assert_eq!(m.hull_point_contents(Hull::Duck, [0.0, 0.0, 18.0]), contents::EMPTY);
    }

    fn spawn_info(spawns: &[Vec3]) -> MapInfo {
        MapInfo { t_spawns: spawns.to_vec(), ..Default::default() }
    }

    #[test]
    fn ground_snap_lands_a_standing_origin_on_the_floor() {
        let m = flat_map();
        let w = World::bare(&m);
        let g = ground_snap(&w, [0.0, 0.0, 100.0]).expect("floor is right there");
        assert!((g[2] - 36.0).abs() < 0.1, "standing origin should be 36 up, got {}", g[2]);
        // Starting below the floor is start_solid, which is not a floor.
        assert_eq!(ground_snap(&w, [0.0, 0.0, -100.0]), None);
        // Starting too high finds nothing within reach.
        assert_eq!(ground_snap(&w, [0.0, 0.0, 5000.0]), None);
    }

    #[test]
    fn flat_ground_classifies_as_a_walk() {
        let m = flat_map();
        let w = World::bare(&m);
        let a = [0.0, 0.0, 36.0];
        let b = [CELL, 0.0, 36.0];
        assert_eq!(classify(&w, a, b), Some(Move::Walk));
        assert_eq!(classify(&w, b, a), Some(Move::Walk));
    }

    #[test]
    fn a_step_beyond_the_engines_limits_is_not_an_edge() {
        let m = flat_map();
        let w = World::bare(&m);
        let a = [0.0, 0.0, 36.0];
        // Above mp_jump_height.
        assert_eq!(classify(&w, a, [CELL, 0.0, 36.0 + 60.0]), None);
        // Below the fall limit.
        assert_eq!(classify(&w, a, [CELL, 0.0, 36.0 - 400.0]), None);
    }

    #[test]
    fn a_rise_within_jump_height_over_open_air_is_a_jump_and_the_reverse_a_fall() {
        let m = flat_map();
        let w = World::bare(&m);
        let a = [0.0, 0.0, 36.0];
        // The rise is inside the walkable grade, but there is no ramp under it
        // -- the floor at the midpoint is a long way below the line -- so this
        // is a jump and not a walk.
        assert_eq!(classify(&w, a, [CELL, 0.0, 36.0 + 40.0]), Some(Move::Jump));
        assert_eq!(classify(&w, [CELL, 0.0, 36.0 + 40.0], a), Some(Move::Fall));
        // A fall is one-way in the sense that the reverse of a *big* drop is
        // not a jump.
        assert_eq!(classify(&w, [CELL, 0.0, 36.0 + 150.0], a), Some(Move::Fall));
        assert_eq!(classify(&w, a, [CELL, 0.0, 36.0 + 150.0]), None);
    }

    /// The doorway from `bsp.rs`, rebuilt with a floor either side, so the
    /// only way through is a crouch.
    fn crouch_corridor() -> Bsp {
        Bsp {
            planes: vec![
                Plane { normal: [0.0, 0.0, 1.0], dist: 36.0, kind: 2 }, // 0
                Plane { normal: [1.0, 0.0, 0.0], dist: 0.0, kind: 0 },  // 1
                Plane { normal: [1.0, 0.0, 0.0], dist: 32.0, kind: 0 }, // 2
                Plane { normal: [0.0, 0.0, 1.0], dist: 18.0, kind: 2 }, // 3
                Plane { normal: [0.0, 0.0, 1.0], dist: 30.0, kind: 2 }, // 4
                Plane { normal: [0.0, 0.0, 1.0], dist: 0.0, kind: 2 },  // 5
            ],
            nodes: vec![Node { plane: 5, children: [-1, -2] }],
            leaves: vec![
                Leaf { contents: contents::EMPTY },
                Leaf { contents: contents::SOLID },
            ],
            clipnodes: vec![
                ClipNode { plane: 0, children: [1, -2] },
                ClipNode { plane: 1, children: [2, -1] },
                ClipNode { plane: 2, children: [-1, -2] },
                ClipNode { plane: 3, children: [4, -2] },
                ClipNode { plane: 1, children: [5, -1] },
                ClipNode { plane: 2, children: [-1, 6] },
                ClipNode { plane: 4, children: [-2, -1] },
            ],
            models: vec![Model {
                mins: [-4096.0; 3],
                maxs: [4096.0; 3],
                origin: [0.0; 3],
                headnode: [0, 0, 0, 3],
            }],
            ..Default::default()
        }
    }

    #[test]
    fn open_ground_measures_the_widest_radius_and_a_wall_shrinks_it() {
        let m = flat_map();
        let w = World::bare(&m);

        // The middle of a 2000-unit room: every scan distance passes, so the
        // sweep runs out of scan distances rather than out of room.
        assert_eq!(node_radius(&w, [0.0, 0.0, 36.0]), MAX_RADIUS);

        // The walls' *expanded* face is at 984. Hard against it there is not
        // even 32 units of room, so the first scan fails and both -16 steps
        // take the answer below zero, where it clamps.
        assert_eq!(node_radius(&w, [960.0, 0.0, 36.0]), 0.0);

        // In between, the answer comes back quantised and never grows as the
        // wall gets closer. That monotonicity is the property worth asserting:
        // a sweep that broke out of the wrong loop would still return legal
        // values, just not ordered ones.
        let mut last = MAX_RADIUS;
        for x in [700.0f32, 800.0, 850.0, 880.0, 900.0, 920.0, 940.0] {
            let r = node_radius(&w, [x, 0.0, 36.0]);
            assert!(
                (0.0..=MAX_RADIUS).contains(&r) && (r / RADIUS_STEP).fract() == 0.0,
                "radius {r} at x={x} is not one of the seven legal values"
            );
            assert!(r <= last, "radius grew from {last} to {r} while approaching the wall");
            last = r;
        }
        assert!(last < MAX_RADIUS, "the wall never shrank the radius at all");
    }

    #[test]
    fn a_low_doorway_classifies_as_a_crouch_not_a_walk() {
        let m = crouch_corridor();
        let w = World::bare(&m);
        // Either side of the 0..32 lintel, on the floor.
        let a = [-40.0, 0.0, 36.0];
        let b = [40.0, 0.0, 36.0];
        assert!(!w.clear(Hull::Stand, a, b), "standing must be blocked");
        assert_eq!(classify(&w, a, b), Some(Move::Crouch));
        assert_eq!(classify(&w, b, a), Some(Move::Crouch));
    }

    /// [`flat_map`] plus a solid crate submodel from (100,-40,0) to
    /// (200,40,80), built the way a `func_wall` appears in a real map: its own
    /// clipnode tree, invisible to the world hull.
    ///
    /// The crate's tree is hull 1's expansion (16 out, 36 down, 36 up); every
    /// hull index points at it, which is fine because the test only ever asks
    /// about [`Hull::Stand`].
    fn crate_map() -> (Bsp, MapInfo) {
        let mut m = flat_map();
        let base = m.planes.len() as u32;
        let (mins, maxs) = ([100.0f32, -40.0, 0.0], [200.0f32, 40.0, 80.0]);
        let hmins = Hull::Stand.mins();
        let hmaxs = Hull::Stand.maxs();
        for (axis, d) in [
            (0usize, mins[0] - hmaxs[0]),
            (0, maxs[0] - hmins[0]),
            (1, mins[1] - hmaxs[1]),
            (1, maxs[1] - hmins[1]),
            (2, mins[2] - hmaxs[2]),
            (2, maxs[2] - hmins[2]),
        ] {
            let mut n = [0.0f32; 3];
            n[axis] = 1.0;
            m.planes.push(Plane { normal: n, dist: d, kind: axis as i32 });
        }
        let root = m.clipnodes.len() as i16;
        // Six half-spaces. Even i is a "low" face: at or past it, keep testing;
        // before it, we are outside the box. Odd i is a "high" face: past it we
        // are outside, before it keep testing. Inside all six is solid.
        for i in 0..6i16 {
            let plane = base + i as u32;
            let children = if i % 2 == 0 {
                [root + i + 1, -1]
            } else if i == 5 {
                [-1, -2]
            } else {
                [-1, root + i + 1]
            };
            m.clipnodes.push(ClipNode { plane, children });
        }
        m.models.push(Model {
            mins,
            maxs,
            origin: [0.0; 3],
            headnode: [0, root as i32, root as i32, root as i32],
        });
        let info = MapInfo {
            t_spawns: vec![[0.0, 0.0, 40.0]],
            solid_brushes: vec![SolidBrush {
                model: 1,
                bounds: Aabb::new(mins, maxs),
                breakable: false,
            }],
            ..Default::default()
        };
        (m, info)
    }

    #[test]
    fn a_solid_brush_entity_blocks_a_trace_the_world_hull_lets_through() {
        let (m, info) = crate_map();
        let a = [0.0, 0.0, 36.0];
        let b = [300.0, 0.0, 36.0];

        // The world hull has never heard of the crate.
        assert!(
            World::bare(&m).clear(Hull::Stand, a, b),
            "the world tree should not contain the brush entity"
        );
        // With the entity in play, the same segment is blocked.
        let w = World::new(&m, &info);
        assert!(!w.clear(Hull::Stand, a, b), "the crate must block the trace");
        let t = w.trace(Hull::Stand, a, b);
        assert!(
            (t.end[0] - 84.0).abs() < 1.0,
            "should stop 16 short of the crate at x=100, got {}",
            t.end[0]
        );
        // And a point inside it is not a place a player fits.
        assert!(!w.fits(Hull::Stand, [150.0, 0.0, 36.0]));
        assert!(w.fits(Hull::Stand, [0.0, 0.0, 36.0]));
        assert!(World::bare(&m).fits(Hull::Stand, [150.0, 0.0, 36.0]));
    }

    #[test]
    fn generation_routes_around_a_solid_brush_entity_instead_of_through_it() {
        let (m, info) = crate_map();
        let inside = Aabb::new([100.0, -40.0, 0.0], [200.0, 40.0, 80.0]);

        let with = NavGrid::generate(&m, &info, 0);
        let mut bare_info = info.clone();
        bare_info.solid_brushes.clear();
        let without = NavGrid::generate(&m, &bare_info, 0);

        let occupied = |g: &NavGrid| {
            g.nodes
                .iter()
                .filter(|n| {
                    let player = Aabb::new(
                        add(n.origin, Hull::Stand.mins()),
                        add(n.origin, Hull::Stand.maxs()),
                    );
                    player.intersects(&inside)
                })
                .count()
        };
        assert!(
            occupied(&without) > 0,
            "without the entity the fill should walk right through the crate"
        );
        assert_eq!(occupied(&with), 0, "no node may overlap a solid brush entity");

        // The far side is still reachable -- the fill goes round, not through.
        let near = with.nearest([0.0, 0.0, 36.0]).expect("a node near the spawn");
        let far = with.nearest([320.0, 0.0, 36.0]).expect("a node past the crate");
        assert!(with.find_path(near, far).is_some(), "the crate cut the map in two");
    }

    #[test]
    fn a_cache_survives_a_trip_through_the_filesystem() {
        let m = flat_map();
        let g = NavGrid::generate(&m, &spawn_info(&[[0.0, 0.0, 40.0]]), 0x1234_5678);
        let path = std::env::temp_dir().join("nav_grid_round_trip.navg");
        g.save(&path).expect("should write");
        let back = NavGrid::load(&path, 0x1234_5678).expect("should read");
        assert_eq!(back, g);
        assert!(matches!(
            NavGrid::load(&path, 0),
            Err(NavError::StaleCache { .. })
        ));
        let _ = std::fs::remove_file(&path);
        assert!(matches!(NavGrid::load(&path, 0), Err(NavError::TooShort)));
    }

    #[test]
    fn generation_on_open_ground_produces_a_connected_lattice() {
        let m = flat_map();
        let info = spawn_info(&[[0.0, 0.0, 40.0]]);
        let g = NavGrid::generate(&m, &info, 7);

        // The floor is unbounded, so the fill must be bounded by something:
        // it is bounded by the world model box, past which the tree is solid.
        assert!(g.len() > 100, "expected a real lattice, got {}", g.len());
        assert!(g.edge_count() >= g.len(), "every node should have links");
        assert_eq!(g.checksum, 7);

        // Everything kept must be reachable from the spawn node.
        let start = g.nearest([0.0, 0.0, 36.0]).expect("a node at the spawn");
        for i in 0..g.len() {
            assert!(
                g.find_path(start, i).is_some(),
                "node {i} at {:?} survived pruning but is unreachable",
                g.nodes[i].origin
            );
        }
    }

    #[test]
    fn every_generated_edge_is_re_admitted_by_classify() {
        let m = flat_map();
        let w = World::bare(&m);
        let g = NavGrid::generate(&m, &spawn_info(&[[0.0, 0.0, 40.0]]), 0);
        for (i, n) in g.nodes.iter().enumerate() {
            for l in &n.links {
                let q = g.nodes[l.to as usize].origin;
                assert_eq!(
                    classify(&w, n.origin, q),
                    Some(l.kind),
                    "edge {i} -> {} was recorded as {:?} but classify disagrees",
                    l.to,
                    l.kind
                );
            }
        }
    }

    #[test]
    fn a_map_with_no_reachable_ground_generates_nothing() {
        let m = flat_map();
        // Spawn far above, out of probe range: nothing snaps, nothing seeds.
        let g = NavGrid::generate(&m, &spawn_info(&[[0.0, 0.0, 100_000.0]]), 0);
        assert!(g.is_empty(), "expected an empty grid, got {} nodes", g.len());
        assert_eq!(g.find_path(0, 0), None);
        assert_eq!(g.nearest([0.0; 3]), None);
    }

    #[test]
    fn objective_volumes_flag_the_nodes_standing_in_them() {
        let m = flat_map();
        let info = MapInfo {
            t_spawns: vec![[0.0, 0.0, 40.0]],
            bomb_sites: vec![Aabb::new([-60.0, -60.0, 0.0], [60.0, 60.0, 96.0])],
            buy_zones: vec![Aabb::new([200.0, -60.0, 0.0], [320.0, 60.0, 96.0])],
            ..Default::default()
        };
        let g = NavGrid::generate(&m, &info, 0);
        let goals = g.nodes_with_flag(flags::GOAL);
        let buys = g.nodes_with_flag(flags::BUY_ZONE);
        assert!(!goals.is_empty(), "the bomb site should have tagged nodes");
        assert!(!buys.is_empty(), "the buy zone should have tagged nodes");
        for i in &goals {
            let o = g.nodes[*i].origin;
            assert!(o[0].abs() <= 60.0 + 16.0 && o[1].abs() <= 60.0 + 16.0, "{o:?}");
        }
        // And the two sets are disjoint here, so a flag is not leaking.
        assert!(goals.iter().all(|i| !buys.contains(i)));
    }

    #[test]
    fn spawn_nodes_are_flagged() {
        let m = flat_map();
        let g = NavGrid::generate(&m, &spawn_info(&[[0.0, 0.0, 40.0]]), 0);
        assert_eq!(g.nodes_with_flag(flags::SPAWN).len(), 1);
    }

    // ------------------------------------------------------------- caching

    #[test]
    fn a_grid_round_trips_through_its_cache_format() {
        let m = flat_map();
        let g = NavGrid::generate(&m, &spawn_info(&[[0.0, 0.0, 40.0]]), 0xdead_beef);
        let bytes = g.to_bytes();
        let back = NavGrid::from_bytes(&bytes, 0xdead_beef).expect("should load");
        assert_eq!(back, g);
        assert_eq!(back.edge_count(), g.edge_count());
    }

    #[test]
    fn a_cache_built_from_a_different_bsp_is_rejected() {
        let m = flat_map();
        let g = NavGrid::generate(&m, &spawn_info(&[[0.0, 0.0, 40.0]]), 1);
        let bytes = g.to_bytes();
        assert_eq!(
            NavGrid::from_bytes(&bytes, 2),
            Err(NavError::StaleCache { expected: 2, found: 1 })
        );
    }

    #[test]
    fn a_corrupt_cache_is_an_error_and_never_a_panic() {
        let g = NavGrid {
            nodes: vec![NavNode {
                origin: [1.0, 2.0, 3.0],
                flags: 0,
                radius: 32.0,
                links: vec![Link { to: 0, kind: Move::Walk }],
            }],
            checksum: 5,
        };
        let good = g.to_bytes();

        assert!(matches!(NavGrid::from_bytes(&[], 5), Err(NavError::TooShort)));
        let mut bad_magic = good.clone();
        bad_magic[0] ^= 0xFF;
        assert!(matches!(NavGrid::from_bytes(&bad_magic, 5), Err(NavError::BadMagic(_))));
        let mut bad_version = good.clone();
        bad_version[4] = 99;
        assert!(matches!(
            NavGrid::from_bytes(&bad_version, 5),
            Err(NavError::BadVersion(99))
        ));
        // Truncation at every length must error, not panic.
        for n in 0..good.len() {
            let _ = NavGrid::from_bytes(&good[..n], 5);
        }
        // A link past the end is caught.
        let mut dangling = good.clone();
        let link_to = good.len() - 5;
        dangling[link_to..link_to + 4].copy_from_slice(&9u32.to_le_bytes());
        assert!(matches!(
            NavGrid::from_bytes(&dangling, 5),
            Err(NavError::LinkOutOfRange { .. })
        ));
        // An unknown move kind is caught.
        let mut bad_kind = good.clone();
        *bad_kind.last_mut().unwrap() = 200;
        assert_eq!(NavGrid::from_bytes(&bad_kind, 5), Err(NavError::BadMoveKind(200)));
    }

    /// The trap the version number exists for.
    ///
    /// A version 1 record is `origin, flags, link count, links`; version 2 put
    /// the radius between `flags` and the link count. Read one as the other and
    /// the first link's index becomes a radius, the link count becomes an
    /// index, and every node after it is misaligned -- a graph that loads
    /// cleanly and is entirely wrong. It has to be refused, not misread.
    #[test]
    fn a_cache_in_the_previous_layout_is_refused_rather_than_misread() {
        let mut v1 = Vec::new();
        v1.extend_from_slice(&NavGrid::MAGIC.to_le_bytes());
        v1.extend_from_slice(&1u32.to_le_bytes());
        v1.extend_from_slice(&7u64.to_le_bytes());
        v1.extend_from_slice(&1u32.to_le_bytes());
        for v in [10.0f32, 20.0, 30.0] {
            v1.extend_from_slice(&v.to_le_bytes());
        }
        v1.extend_from_slice(&flags::GOAL.to_le_bytes());
        v1.extend_from_slice(&1u32.to_le_bytes()); // one link
        v1.extend_from_slice(&0u32.to_le_bytes());
        v1.push(Move::Walk.to_byte());

        assert_eq!(NavGrid::from_bytes(&v1, 7), Err(NavError::BadVersion(1)));
        // And the version 2 writer really did move the layout: same one-node
        // grid, four bytes longer.
        let v2 = NavGrid {
            nodes: vec![NavNode {
                origin: [10.0, 20.0, 30.0],
                flags: flags::GOAL,
                radius: 0.0,
                links: vec![Link { to: 0, kind: Move::Walk }],
            }],
            checksum: 7,
        }
        .to_bytes();
        assert_eq!(v2.len(), v1.len() + 4);
    }

    #[test]
    fn the_checksum_separates_maps_that_differ_by_one_byte() {
        let a = checksum(b"the same bytes");
        let b = checksum(b"the same byteS");
        assert_ne!(a, b);
        assert_eq!(a, checksum(b"the same bytes"));
        assert_ne!(checksum(b""), 0);
    }

    #[test]
    fn every_move_kind_survives_a_byte_round_trip() {
        for m in [
            Move::Walk,
            Move::Jump,
            Move::Fall,
            Move::Crouch,
            Move::Ladder,
            Move::Break,
        ] {
            assert_eq!(Move::from_byte(m.to_byte()), Some(m));
            assert!(m.cost_multiplier() >= 1.0, "{m:?} would break A* admissibility");
        }
        assert_eq!(Move::from_byte(6), None);
    }

    // -------------------------------------------------------- the real maps

    fn maps_dir() -> PathBuf {
        match std::env::var("AIPLAYERS_MAPS") {
            Ok(d) => PathBuf::from(d),
            Err(_) => PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("testserver")
                .join("cstrike")
                .join("maps"),
        }
    }

    /// Load a shipped map, or print a SKIP line saying exactly what is missing.
    ///
    /// The maps are gitignored (`harness setup` pulls them out of the HLDS
    /// container), so these tests have to be skippable — but a test that
    /// returns in silence is indistinguishable from one that passed, which is
    /// the whole reason this prints.
    fn real_map(name: &str) -> Option<Bsp> {
        let path = maps_dir().join(format!("{name}.bsp"));
        if !path.exists() {
            eprintln!("SKIP: {} not found (set AIPLAYERS_MAPS)", path.display());
            return None;
        }
        let data = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        Some(
            Bsp::parse(&data)
                .unwrap_or_else(|e| panic!("failed to parse {}: {e}", path.display())),
        )
    }

    type Generated = Arc<(Bsp, MapInfo, NavGrid)>;

    /// Generate once per map per test run.
    ///
    /// Several tests need de_dust2's grid and generation is not cheap in a
    /// debug build; without this the suite would build the same graph five
    /// times over.
    fn real_grid(name: &str) -> Option<Generated> {
        static CACHE: OnceLock<Mutex<HashMap<String, Option<Generated>>>> = OnceLock::new();
        let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hit) = guard.get(name) {
            return hit.clone();
        }
        let built = (|| {
            let bsp = real_map(name)?;
            let info = MapInfo::from_bsp(&bsp)
                .unwrap_or_else(|e| panic!("{name}: could not derive MapInfo: {e}"));
            let started = Instant::now();
            let grid = NavGrid::generate(&bsp, &info, 1234);
            eprintln!(
                "{name}: {} nodes, {} edges, scenario {:?}, generated in {:.1}s",
                grid.len(),
                grid.edge_count(),
                info.scenario,
                started.elapsed().as_secs_f32(),
            );
            Some(Arc::new((bsp, info, grid)))
        })();
        guard.insert(name.to_string(), built.clone());
        built
    }

    #[test]
    fn de_dust2_is_a_bomb_map_with_two_sites() {
        let Some(bsp) = real_map("de_dust2") else { return };
        let info = MapInfo::from_bsp(&bsp).expect("should derive");
        assert_eq!(info.scenario, Scenario::Bomb);
        assert_eq!(info.bomb_sites.len(), 2, "de_dust2 has A and B");
        assert!(!info.buy_zones.is_empty(), "de_dust2 has buy zones");
        assert!(info.t_spawns.len() >= 8, "got {}", info.t_spawns.len());
        assert!(info.ct_spawns.len() >= 8, "got {}", info.ct_spawns.len());
        assert!(info.rescue_zones.is_empty(), "a de_ map has no rescue zone");
        // The two sites must not be the same box.
        assert_ne!(info.bomb_sites[0], info.bomb_sites[1]);
    }

    #[test]
    fn a_cs_map_is_a_hostage_map_with_a_rescue_zone() {
        let Some(bsp) = real_map("cs_office") else { return };
        let info = MapInfo::from_bsp(&bsp).expect("should derive");
        assert_eq!(info.scenario, Scenario::Hostage);
        assert!(!info.rescue_zones.is_empty());
        assert!(!info.hostage_spawns.is_empty(), "hostages to rescue");
        assert!(info.bomb_sites.is_empty());
        assert!(!info.ladders.is_empty(), "cs_office has two func_ladder");
    }

    #[test]
    fn every_shipped_map_derives_a_map_info() {
        let dir = maps_dir();
        if !dir.is_dir() {
            eprintln!("SKIP: {} is not a directory", dir.display());
            return;
        }
        let mut seen = 0;
        for entry in std::fs::read_dir(&dir).expect("readable") {
            let path = entry.expect("entry").path();
            if path.extension().map(|e| e != "bsp").unwrap_or(true) {
                continue;
            }
            let data = std::fs::read(&path).expect("readable");
            let bsp = Bsp::parse(&data)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let info = MapInfo::from_bsp(&bsp)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            eprintln!(
                "{:<16} {:?} bomb={} rescue={} buy={} ladders={} T={} CT={} hostages={}",
                path.file_stem().unwrap().to_string_lossy(),
                info.scenario,
                info.bomb_sites.len(),
                info.rescue_zones.len(),
                info.buy_zones.len(),
                info.ladders.len(),
                info.t_spawns.len(),
                info.ct_spawns.len(),
                info.hostage_spawns.len(),
            );
            seen += 1;
        }
        if seen == 0 {
            eprintln!("SKIP: no .bsp files in {}", dir.display());
        }
    }

    /// Generation must survive every map in the rotation, not just the three
    /// the other tests poke at, and on the large majority of them both teams'
    /// spawns must end up in one component.
    ///
    /// Not *all* of them, and the exceptions are known rather than tolerated.
    /// cs_747, cs_backalley and cs_siege each come out split at a level
    /// transition -- a stairwell or a gateway that the fixed 40-unit lattice
    /// never places a node inside, so the two floors never meet. It is the
    /// lattice and not the collision model: regenerating those three with every
    /// brush entity removed leaves them exactly as split. Sub-cell probing is
    /// the fix and is not done here. The ratio asserted below is what turns
    /// "three known maps" into a regression guard -- break the fill and it
    /// falls straight through.
    #[test]
    fn every_shipped_map_generates_a_usable_grid() {
        let dir = maps_dir();
        if !dir.is_dir() {
            eprintln!("SKIP: {} is not a directory", dir.display());
            return;
        }
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .expect("readable")
            .filter_map(|e| {
                let p = e.ok()?.path();
                (p.extension()? == "bsp").then(|| p.file_stem()?.to_str().map(str::to_string))?
            })
            .collect();
        names.sort();
        if names.is_empty() {
            eprintln!("SKIP: no .bsp files in {}", dir.display());
            return;
        }

        let (mut checked, mut connected) = (0usize, 0usize);
        let mut split: Vec<String> = Vec::new();
        for name in &names {
            let data = std::fs::read(dir.join(format!("{name}.bsp"))).expect("readable");
            let bsp = Bsp::parse(&data).unwrap_or_else(|e| panic!("{name}: {e}"));
            let info = MapInfo::from_bsp(&bsp).unwrap_or_else(|e| panic!("{name}: {e}"));
            let started = Instant::now();
            let grid = NavGrid::generate(&bsp, &info, checksum(&data));
            eprintln!(
                "{name:<16} {:>6} nodes {:>7} edges  {:.1}s",
                grid.len(),
                grid.edge_count(),
                started.elapsed().as_secs_f32()
            );
            assert!(grid.len() > 200, "{name}: only {} nodes", grid.len());
            assert!(grid.len() <= MAX_NODES, "{name}: hit the node cap");

            if info.t_spawns.is_empty() || info.ct_spawns.is_empty() {
                continue;
            }
            let t = grid.nearest(info.t_spawns[0]).expect("a node near the T spawn");
            let ct = grid.nearest(info.ct_spawns[0]).expect("a node near the CT spawn");
            checked += 1;
            if grid.find_path(t, ct).is_some() {
                connected += 1;
            } else {
                split.push(name.clone());
                eprintln!("   ^ SPLIT: the two spawns are in different components");
            }
        }

        eprintln!("spawns connected on {connected}/{checked} maps; split: {split:?}");
        assert!(
            connected * 5 >= checked * 4,
            "only {connected} of {checked} maps connect both spawns: {split:?}"
        );
    }

    #[test]
    fn de_dust2_generates_a_few_thousand_nodes() {
        let Some(g) = real_grid("de_dust2") else { return };
        let grid = &g.2;
        assert!(
            grid.len() > 1000,
            "expected a few thousand nodes on de_dust2, got {}",
            grid.len()
        );
        assert!(grid.len() < 60_000, "suspiciously many nodes: {}", grid.len());
        assert!(grid.edge_count() > grid.len() * 2, "the graph is barely connected");
    }

    /// The correctness check that matters: re-run the *same* predicate that
    /// admitted each edge, on every consecutive pair of an actual route.
    #[test]
    fn every_step_of_a_real_route_is_re_admitted_by_the_engine_hull() {
        let Some(g) = real_grid("de_dust2") else { return };
        let (bsp, info, grid) = (&g.0, &g.1, &g.2);
        let world = World::new(bsp, info);
        let t = grid
            .nearest(info.t_spawns[0])
            .expect("a node near the T spawn");
        let goal = grid
            .nearest(info.bomb_sites[0].centre())
            .expect("a node near site A");
        let path = grid.find_path(t, goal).expect("T spawn should reach site A");
        assert!(path.len() > 5, "suspiciously short path: {}", path.len());

        for w in path.windows(2) {
            let (a, b) = (grid.nodes[w[0]].origin, grid.nodes[w[1]].origin);
            let recorded = grid.move_between(w[0], w[1]).expect("path used a real edge");
            assert_eq!(
                classify(&world, a, b),
                Some(recorded),
                "step {} -> {} ({a:?} -> {b:?}) is not a legal move",
                w[0],
                w[1]
            );
        }
    }

    /// Independent of [`classify`]: whatever route admitted a node, a standing
    /// player has to fit there. This one would catch a bug the round-trip
    /// through `classify` cannot, because it does not use `classify` at all.
    #[test]
    fn no_generated_node_is_inside_solid() {
        for name in ["de_dust2", "cs_office", "cs_assault"] {
            let Some(g) = real_grid(name) else { return };
            let (bsp, info, grid) = (&g.0, &g.1, &g.2);
            let world = World::new(bsp, info);
            for (i, n) in grid.nodes.iter().enumerate() {
                assert!(
                    world.fits(Hull::Stand, n.origin),
                    "{name}: node {i} at {:?} is inside solid",
                    n.origin
                );
            }
        }
    }

    /// Every node that is not hanging on a ladder must be standing on
    /// something.
    ///
    /// The probe goes straight *down* from the origin rather than through
    /// [`ground_snap`], which lifts by a step first: a legal standing spot with
    /// less than 18 units of headroom -- de_dust2 has them -- would make that
    /// lift begin inside the ceiling and report "no floor" for a node that is
    /// firmly on the ground.
    #[test]
    fn no_generated_node_is_floating() {
        for name in ["de_dust2", "cs_office"] {
            let Some(g) = real_grid(name) else { return };
            let (bsp, info, grid) = (&g.0, &g.1, &g.2);
            let world = World::new(bsp, info);
            for (i, n) in grid.nodes.iter().enumerate() {
                if n.flags & flags::LADDER != 0 {
                    continue; // ladder nodes hang on purpose
                }
                // Breakables are ignored on purpose: a node behind a shot-out
                // window is standing on real floor, and World::fits and the
                // ground probe both take that view.
                let below = [n.origin[0], n.origin[1], n.origin[2] - 8.0];
                let t = world.trace_ignoring_breakables(Hull::Stand, n.origin, below);
                assert!(!t.start_solid, "{name}: node {i} at {:?} is in solid", n.origin);
                assert!(
                    t.fraction < 1.0 && (t.end[2] - n.origin[2]).abs() < 1.0,
                    "{name}: node {i} at {:?} is floating (stopped at {:?})",
                    n.origin,
                    t.end
                );
            }
        }
    }

    /// The low-headroom case that broke the first version of the fill.
    #[test]
    fn a_floor_under_a_low_ceiling_is_still_found() {
        let m = flat_map();
        let w = World::bare(&m);
        // The synthetic map has no ceiling, so all three probe heights agree.
        let a = w.floor_in_window(0.0, 0.0, 36.0).expect("floor");
        assert!((a[2] - 36.0).abs() < 0.1);
        // Under a real low ceiling, the lifted probes start in solid and the
        // level one is the only thing that finds the floor. crouch_corridor's
        // lintel spans 0..32 and its hull-1 tree is solid from z = 12 up, so a
        // probe from 36 + 44 is inside it.
        let c = crouch_corridor();
        let cw = World::bare(&c);
        assert!(
            cw.trace(
                Hull::Stand,
                [16.0, 0.0, 36.0 + MAX_JUMP],
                [16.0, 0.0, 36.0 - MAX_FALL],
            )
            .start_solid,
            "the lifted probe should start inside the lintel"
        );
        assert!(
            cw.floor_in_window(-40.0, 0.0, 36.0).is_some(),
            "the open corridor either side must still find its floor"
        );
    }

    /// Every recorded edge on a real map, not just the ones on one route.
    #[test]
    fn every_edge_on_a_real_map_is_re_admitted() {
        let Some(g) = real_grid("cs_assault") else { return };
        let (bsp, info, grid) = (&g.0, &g.1, &g.2);
        let world = World::new(bsp, info);
        let mut checked = 0;
        for (i, n) in grid.nodes.iter().enumerate() {
            for l in &n.links {
                let q = grid.nodes[l.to as usize].origin;
                if l.kind == Move::Ladder {
                    // classify() does not produce ladder moves; a ladder link
                    // is a vertical hop up the same lattice column.
                    assert!(
                        (n.origin[0] - q[0]).abs() < 0.01 && (n.origin[1] - q[1]).abs() < 0.01,
                        "ladder link {i} -> {} is not vertical",
                        l.to
                    );
                    assert!(
                        world.clear(Hull::Stand, n.origin, q),
                        "ladder link {i} -> {} passes through solid",
                        l.to
                    );
                } else {
                    assert_eq!(
                        classify(&world, n.origin, q),
                        Some(l.kind),
                        "edge {i} -> {} recorded as {:?}",
                        l.to,
                        l.kind
                    );
                }
                checked += 1;
            }
        }
        eprintln!("cs_assault: re-verified {checked} edges");
        assert!(checked > 1000);
    }

    #[test]
    fn every_spawn_and_objective_on_de_dust2_has_a_node_near_it() {
        let Some(g) = real_grid("de_dust2") else { return };
        let (info, grid) = (&g.1, &g.2);
        let check = |label: &str, p: Vec3| {
            let i = grid.nearest(p).expect("a non-empty grid");
            let d = route::dist(grid.nodes[i].origin, p);
            assert!(d < 96.0, "{label} at {p:?} is {d:.0} units from any node");
        };
        for (n, p) in info.t_spawns.iter().enumerate() {
            check(&format!("T spawn {n}"), *p);
        }
        for (n, p) in info.ct_spawns.iter().enumerate() {
            check(&format!("CT spawn {n}"), *p);
        }
        for (n, z) in info.bomb_sites.iter().enumerate() {
            check(&format!("bomb site {n}"), z.centre());
        }
        for (n, z) in info.buy_zones.iter().enumerate() {
            check(&format!("buy zone {n}"), z.centre());
        }
    }

    #[test]
    fn the_spawns_of_the_two_teams_are_connected_to_each_other() {
        let Some(g) = real_grid("de_dust2") else { return };
        let (info, grid) = (&g.1, &g.2);
        let t = grid.nearest(info.t_spawns[0]).unwrap();
        let ct = grid.nearest(info.ct_spawns[0]).unwrap();
        assert!(grid.find_path(t, ct).is_some(), "T spawn cannot reach CT spawn");
        assert!(grid.find_path(ct, t).is_some(), "CT spawn cannot reach T spawn");
    }

    #[test]
    fn a_ct_can_reach_every_hostage_on_cs_office() {
        let Some(g) = real_grid("cs_office") else { return };
        let (info, grid) = (&g.1, &g.2);
        let from = grid.nearest(info.ct_spawns[0]).unwrap();
        for (n, h) in info.hostage_spawns.iter().enumerate() {
            let to = grid.nearest(*h).expect("a node near the hostage");
            let d = route::dist(grid.nodes[to].origin, *h);
            assert!(d < 128.0, "hostage {n} at {h:?} is {d:.0} units from any node");
            assert!(grid.find_path(from, to).is_some(), "no route to hostage {n}");
        }
    }

    #[test]
    fn the_move_mix_on_a_real_map_is_mostly_walking() {
        let Some(g) = real_grid("de_dust2") else { return };
        let grid = &g.2;
        let mut counts = [0usize; 6];
        for n in &grid.nodes {
            for l in &n.links {
                counts[l.kind.to_byte() as usize] += 1;
            }
        }
        eprintln!(
            "de_dust2 moves: walk={} jump={} fall={} crouch={} ladder={} break={}",
            counts[0], counts[1], counts[2], counts[3], counts[4], counts[5]
        );
        let total: usize = counts.iter().sum();
        assert!(
            counts[0] * 2 > total,
            "walking should dominate a flat map, got {counts:?}"
        );
    }

    #[test]
    fn both_teams_can_reach_both_bomb_sites_on_de_dust2() {
        let Some(g) = real_grid("de_dust2") else { return };
        let (info, grid) = (&g.1, &g.2);
        for (team, spawns) in [("T", &info.t_spawns), ("CT", &info.ct_spawns)] {
            let from = grid.nearest(spawns[0]).expect("a node near the spawn");
            for (n, site) in info.bomb_sites.iter().enumerate() {
                let to = grid.nearest(site.centre()).expect("a node near the site");
                let path = grid.find_path(from, to);
                assert!(
                    path.is_some(),
                    "{team} spawn cannot reach bomb site {n} at {:?}",
                    site.centre()
                );
                eprintln!(
                    "{team} spawn -> site {n}: {} nodes",
                    path.unwrap().len()
                );
            }
        }
    }

    #[test]
    fn the_bomb_sites_on_de_dust2_have_flagged_nodes() {
        let Some(g) = real_grid("de_dust2") else { return };
        let grid = &g.2;
        let goals = grid.nodes_with_flag(flags::GOAL);
        let buys = grid.nodes_with_flag(flags::BUY_ZONE);
        assert!(goals.len() > 4, "only {} nodes on a bomb site", goals.len());
        assert!(buys.len() > 4, "only {} nodes in a buy zone", buys.len());
    }

    #[test]
    fn a_real_grid_round_trips_through_the_cache() {
        let Some(g) = real_grid("de_dust2") else { return };
        let grid = &g.2;
        let bytes = grid.to_bytes();
        let back = NavGrid::from_bytes(&bytes, grid.checksum).expect("should load");
        assert_eq!(&back, grid);
        assert_eq!(
            NavGrid::from_bytes(&bytes, grid.checksum ^ 1),
            Err(NavError::StaleCache {
                expected: grid.checksum ^ 1,
                found: grid.checksum
            })
        );
    }

    #[test]
    fn a_hostage_map_routes_from_a_ct_spawn_to_a_rescue_zone() {
        let Some(g) = real_grid("cs_office") else { return };
        let (info, grid) = (&g.1, &g.2);
        let from = grid.nearest(info.ct_spawns[0]).expect("a node near the spawn");
        let to = grid
            .nearest(info.rescue_zones[0].centre())
            .expect("a node near the rescue zone");
        assert!(
            grid.find_path(from, to).is_some(),
            "a CT must be able to walk to the rescue zone"
        );
        assert!(!grid.nodes_with_flag(flags::RESCUE).is_empty());
    }

    /// What the sweep actually found on a real map.
    ///
    /// Prints the histogram, because "the radius is computed" and "the radius
    /// is useful" are different claims: a sweep that returned 0 everywhere
    /// would pass every other assertion here and silently disable both the
    /// destination jitter and the path smoothing that depend on it.
    #[test]
    fn de_dust2_nodes_get_a_spread_of_wayzone_radii() {
        let Some(g) = real_grid("de_dust2") else { return };
        let grid = &g.2;

        let mut hist: std::collections::BTreeMap<i32, usize> = std::collections::BTreeMap::new();
        for n in &grid.nodes {
            *hist.entry(n.radius as i32).or_default() += 1;
        }
        eprintln!(
            "de_dust2 radius histogram over {} nodes: {:?}",
            grid.len(),
            hist
        );

        for (i, n) in grid.nodes.iter().enumerate() {
            assert!(
                (0.0..=MAX_RADIUS).contains(&n.radius)
                    && (n.radius / RADIUS_STEP).fract() == 0.0,
                "node {i} has radius {}, which is not one of {{0,16,..,96}}",
                n.radius
            );
        }

        // The classes that must be exact.
        for flag in [flags::GOAL, flags::LADDER] {
            for i in grid.nodes_with_flag(flag) {
                assert_eq!(
                    grid.nodes[i].radius, 0.0,
                    "node {i} carries flag {flag:#x} and must be arrived at exactly"
                );
            }
        }

        let open = grid.nodes.iter().filter(|n| n.radius > 0.0).count();
        assert!(
            open * 4 >= grid.len(),
            "only {open} of {} nodes have any room around them; the sweep is \
             measuring something wrong",
            grid.len()
        );
    }

    /// The zig-zag, and what smoothing does to it.
    ///
    /// A 40-unit lattice cannot represent a diagonal, so A\* returns a
    /// staircase and a bot that steers at every step walks the staircase. The
    /// acceptance numbers are from the humanisation plan: at most 60 % of the
    /// raw node count, and nothing further apart than 400 units.
    #[test]
    fn post_smoothing_takes_the_zig_zag_out_of_a_real_route() {
        let Some(g) = real_grid("de_dust2") else { return };
        let (bsp, info, grid) = (&g.0, &g.1, &g.2);
        let world = World::new(bsp, info);

        let from = grid.nearest(info.t_spawns[0]).expect("a node near the T spawn");
        let to = grid
            .nearest(info.bomb_sites[0].centre())
            .expect("a node near site A");
        let raw = grid.find_path(from, to).expect("T spawn should reach site A");
        let smooth = grid.smooth_path(&raw);
        eprintln!(
            "de_dust2 T spawn -> site A: {} raw nodes -> {} smoothed ({} %)",
            raw.len(),
            smooth.len(),
            smooth.len() * 100 / raw.len()
        );

        assert_eq!(smooth.first(), raw.first(), "smoothing moved the start");
        assert_eq!(smooth.last(), raw.last(), "smoothing moved the destination");
        assert!(
            smooth.len() * 100 <= raw.len() * 60,
            "smoothing kept {} of {} nodes, over the 60 % budget",
            smooth.len(),
            raw.len()
        );

        // A subsequence, never a re-route: smoothing may drop nodes and must
        // not invent one.
        let mut raw_iter = raw.iter();
        assert!(
            smooth.iter().all(|s| raw_iter.any(|r| r == s)),
            "the smoothed path is not a subsequence of the raw one"
        );

        let mut new_lines = 0;
        for w in smooth.windows(2) {
            let (a, b) = (grid.nodes[w[0]].origin, grid.nodes[w[1]].origin);
            let d = route::dist(a, b);
            assert!(
                d <= SKIP_MAX_DIST,
                "smoothed hop {} -> {} is {d:.0} units, past the {SKIP_MAX_DIST} limit",
                w[0],
                w[1]
            );
            // Every hop is either an edge the graph already believed in, or a
            // *new* straight line the corridor test invented -- and the second
            // kind has to be held to the engine's answer, because the corridor
            // test is only a stand-in for YaPB's traced visibility table. Hull
            // 3 at a standing origin spans waist to shoulders, so a kerb inside
            // `sv_stepsize` -- which a player walks over -- is not an
            // obstruction, but a wall is.
            //
            // The distinction matters: an edge can legitimately be a *fall*,
            // where the straight line leaves the ledge and passes through the
            // wall below it. Asserting a clear line on those would be asserting
            // something untrue about a route the bot has always walked.
            let recorded = grid.move_between(w[0], w[1]).is_some();
            assert!(
                recorded || world.trace(Hull::Duck, a, b).is_clear(),
                "smoothed hop {} -> {} ({a:?} -> {b:?}) is neither an edge nor a \
                 clear line",
                w[0],
                w[1]
            );
            new_lines += usize::from(!recorded);
        }
        // Without this the trace check above could pass by never running.
        assert!(
            new_lines > 5,
            "only {new_lines} hops were genuinely new straight lines"
        );
    }

    #[test]
    fn a_ladder_map_produces_ladder_nodes_and_links() {
        let Some(g) = real_grid("cs_assault") else { return };
        let (info, grid) = (&g.1, &g.2);
        assert_eq!(info.ladders.len(), 8, "cs_assault has eight func_ladder");
        let ladder_nodes = grid.nodes_with_flag(flags::LADDER);
        assert!(
            !ladder_nodes.is_empty(),
            "no ladder node survived on a map with eight ladders"
        );
        let ladder_links: usize = grid
            .nodes
            .iter()
            .map(|n| n.links.iter().filter(|l| l.kind == Move::Ladder).count())
            .sum();
        eprintln!("cs_assault: {} ladder nodes, {ladder_links} ladder links", ladder_nodes.len());
        assert!(ladder_links > 0, "ladder nodes exist but nothing climbs them");
    }
}
