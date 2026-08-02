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

use crate::bsp::{Bsp, Hull, Vec3};
use crate::entities::{Aabb, MapInfo};
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

/// Vertical spacing of the nodes on a ladder.
pub const LADDER_STEP: f32 = 32.0;

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
        }
    }

    fn to_byte(self) -> u8 {
        match self {
            Self::Walk => 0,
            Self::Jump => 1,
            Self::Fall => 2,
            Self::Crouch => 3,
            Self::Ladder => 4,
        }
    }

    fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            0 => Self::Walk,
            1 => Self::Jump,
            2 => Self::Fall,
            3 => Self::Crouch,
            4 => Self::Ladder,
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

/// Drop a standing player onto the floor under `p`.
///
/// Probes from `p + (0,0,18)` — one step's worth of slack, so a position a
/// little inside the floor still snaps — down to `p - (0,0,200)`. Returns the
/// legal standing **origin**, or `None` if the start is already inside solid or
/// there is no floor within reach.
pub fn ground_snap(bsp: &Bsp, p: Vec3) -> Option<Vec3> {
    drop_to_floor(bsp, p[0], p[1], p[2] + STEP_SIZE, STEP_SIZE + MAX_FALL)
}

fn drop_to_floor(bsp: &Bsp, x: f32, y: f32, from_z: f32, distance: f32) -> Option<Vec3> {
    let t = bsp.hull_trace(Hull::Stand, [x, y, from_z], [x, y, from_z - distance]);
    if t.start_solid || t.fraction >= 1.0 {
        None
    } else {
        Some(t.end)
    }
}

/// The highest floor in the column `(x, y)` between `from + MAX_JUMP` and
/// `from - MAX_FALL` — the whole band one move can reach.
///
/// Three starts, not one. Lifting the probe by a jump's worth is what finds a
/// ledge you could jump onto, but under a low ceiling that lift begins *inside*
/// solid, and "the probe started in a wall" is not the same fact as "there is
/// no floor here". Dropping back to a step's worth and then to level with the
/// source recovers those columns. de_dust2 has a node under exactly such a
/// ceiling, which is how this was found.
fn floor_in_window(bsp: &Bsp, x: f32, y: f32, from: f32) -> Option<Vec3> {
    for lift in [MAX_JUMP, STEP_SIZE, 0.0] {
        if let Some(g) = drop_to_floor(bsp, x, y, from + lift, lift + MAX_FALL) {
            return Some(g);
        }
    }
    None
}

fn clear(bsp: &Bsp, hull: Hull, a: Vec3, b: Vec3) -> bool {
    bsp.hull_trace(hull, a, b).is_clear()
}

/// A horizontal move with the engine's step-up.
///
/// `PM_StepUp` does not give up when the direct move is blocked: it lifts the
/// player by `sv_stepsize`, moves, and drops back down (`pm_shared.cpp:1196`,
/// `:1214`). Without modelling that, every kerb in the map would read as a
/// wall, because the expanded hull turns a 4-unit step into a 4-unit cliff face
/// sitting 16 units out from the real one.
fn step_move(bsp: &Bsp, hull: Hull, a: Vec3, b: Vec3) -> bool {
    if clear(bsp, hull, a, b) {
        return true;
    }
    let ah = raise(a, STEP_SIZE);
    let bh = raise(b, STEP_SIZE);
    clear(bsp, hull, a, ah) && clear(bsp, hull, ah, bh) && clear(bsp, hull, bh, b)
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
pub fn classify(bsp: &Bsp, from: Vec3, to: Vec3) -> Option<Move> {
    let dz = to[2] - from[2];

    if dz.abs() <= STEP_SIZE {
        if step_move(bsp, Hull::Stand, from, to) {
            return Some(Move::Walk);
        }
        // Standing does not fit. A ducking origin sits 18 above the feet
        // instead of 36, so drop both ends by 18 to keep the feet where they
        // were and ask hull 3 the same question.
        let d = Hull::Stand.eye_to_feet() - Hull::Duck.eye_to_feet();
        if step_move(bsp, Hull::Duck, raise(from, -d), raise(to, -d)) {
            return Some(Move::Crouch);
        }
        return None;
    }

    if dz > STEP_SIZE && dz <= MAX_JUMP {
        // Rise straight up first -- that vertical trace is the head-clearance
        // check -- and only then move across.
        let apex = [from[0], from[1], to[2]];
        if clear(bsp, Hull::Stand, from, apex) && clear(bsp, Hull::Stand, apex, to) {
            return Some(Move::Jump);
        }
        return None;
    }

    if dz < -STEP_SIZE && dz >= -MAX_FALL {
        // Walk off the ledge, then fall. If a railing blocks the first leg
        // there is nothing to fall from.
        let over = [to[0], to[1], from[2]];
        if clear(bsp, Hull::Stand, from, over) && clear(bsp, Hull::Stand, over, to) {
            return Some(Move::Fall);
        }
        return None;
    }

    None
}

// ---------------------------------------------------------------- building

struct Builder<'a> {
    bsp: &'a Bsp,
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
    fn new(bsp: &'a Bsp) -> Self {
        let w = bsp.models[0];
        Self {
            bsp,
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
        self.nodes.push(NavNode { origin, flags, links: Vec::new() });
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
            if let Some(g) =
                drop_to_floor(self.bsp, x, y, p[2] + STEP_SIZE, STEP_SIZE + MAX_FALL)
            {
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
            if self.bsp.hull_point_contents(Hull::Stand, p) != crate::bsp::contents::SOLID {
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
            if clear(self.bsp, Hull::Stand, pa, pb) {
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
            if self.bsp.hull_point_contents(Hull::Stand, [x, y, z])
                != crate::bsp::contents::SOLID
            {
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
                let found = floor_in_window(self.bsp, x, y, p[2]);
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
                        if dz <= MAX_JUMP && dz >= -MAX_FALL && !candidates.contains(&j) {
                            candidates.push(j);
                        }
                    }
                }

                for j in candidates {
                    if j == i {
                        continue;
                    }
                    let q = self.nodes[j as usize].origin;
                    let Some(kind) = classify(self.bsp, p, q) else {
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
        let mut b = Builder::new(bsp);

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
    pub const VERSION: u32 = 1;

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(24 + self.nodes.len() * 24);
        out.extend_from_slice(&Self::MAGIC.to_le_bytes());
        out.extend_from_slice(&Self::VERSION.to_le_bytes());
        out.extend_from_slice(&self.checksum.to_le_bytes());
        out.extend_from_slice(&(self.nodes.len() as u32).to_le_bytes());
        for n in &self.nodes {
            for v in n.origin {
                out.extend_from_slice(&v.to_le_bytes());
            }
            out.extend_from_slice(&n.flags.to_le_bytes());
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
            nodes.push(NavNode { origin, flags, links });
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
        let g = ground_snap(&m, [0.0, 0.0, 100.0]).expect("floor is right there");
        assert!((g[2] - 36.0).abs() < 0.1, "standing origin should be 36 up, got {}", g[2]);
        // Starting below the floor is start_solid, which is not a floor.
        assert_eq!(ground_snap(&m, [0.0, 0.0, -100.0]), None);
        // Starting too high finds nothing within reach.
        assert_eq!(ground_snap(&m, [0.0, 0.0, 5000.0]), None);
    }

    #[test]
    fn flat_ground_classifies_as_a_walk() {
        let m = flat_map();
        let a = [0.0, 0.0, 36.0];
        let b = [CELL, 0.0, 36.0];
        assert_eq!(classify(&m, a, b), Some(Move::Walk));
        assert_eq!(classify(&m, b, a), Some(Move::Walk));
    }

    #[test]
    fn a_step_beyond_the_engines_limits_is_not_an_edge() {
        let m = flat_map();
        let a = [0.0, 0.0, 36.0];
        // Above mp_jump_height.
        assert_eq!(classify(&m, a, [CELL, 0.0, 36.0 + 60.0]), None);
        // Below the fall limit.
        assert_eq!(classify(&m, a, [CELL, 0.0, 36.0 - 400.0]), None);
    }

    #[test]
    fn a_rise_within_jump_height_over_open_air_is_a_jump_and_the_reverse_a_fall() {
        let m = flat_map();
        let a = [0.0, 0.0, 36.0];
        assert_eq!(classify(&m, a, [CELL, 0.0, 36.0 + 40.0]), Some(Move::Jump));
        assert_eq!(classify(&m, [CELL, 0.0, 36.0 + 40.0], a), Some(Move::Fall));
        // A fall is one-way in the sense that the reverse of a *big* drop is
        // not a jump.
        assert_eq!(classify(&m, [CELL, 0.0, 36.0 + 150.0], a), Some(Move::Fall));
        assert_eq!(classify(&m, a, [CELL, 0.0, 36.0 + 150.0]), None);
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
    fn a_low_doorway_classifies_as_a_crouch_not_a_walk() {
        let m = crouch_corridor();
        // Either side of the 0..32 lintel, on the floor.
        let a = [-40.0, 0.0, 36.0];
        let b = [40.0, 0.0, 36.0];
        assert!(!clear(&m, Hull::Stand, a, b), "standing must be blocked");
        assert_eq!(classify(&m, a, b), Some(Move::Crouch));
        assert_eq!(classify(&m, b, a), Some(Move::Crouch));
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
        let g = NavGrid::generate(&m, &spawn_info(&[[0.0, 0.0, 40.0]]), 0);
        for (i, n) in g.nodes.iter().enumerate() {
            for l in &n.links {
                let q = g.nodes[l.to as usize].origin;
                assert_eq!(
                    classify(&m, n.origin, q),
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
        for m in [Move::Walk, Move::Jump, Move::Fall, Move::Crouch, Move::Ladder] {
            assert_eq!(Move::from_byte(m.to_byte()), Some(m));
            assert!(m.cost_multiplier() >= 1.0, "{m:?} would break A* admissibility");
        }
        assert_eq!(Move::from_byte(9), None);
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
                classify(&bsp, a, b),
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
            let (bsp, grid) = (&g.0, &g.2);
            for (i, n) in grid.nodes.iter().enumerate() {
                assert_ne!(
                    bsp.hull_point_contents(Hull::Stand, n.origin),
                    contents::SOLID,
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
            let (bsp, grid) = (&g.0, &g.2);
            for (i, n) in grid.nodes.iter().enumerate() {
                if n.flags & flags::LADDER != 0 {
                    continue; // ladder nodes hang on purpose
                }
                let below = [n.origin[0], n.origin[1], n.origin[2] - 8.0];
                let t = bsp.hull_trace(Hull::Stand, n.origin, below);
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
        // The synthetic map has no ceiling, so all three probe heights agree.
        let a = floor_in_window(&m, 0.0, 0.0, 36.0).expect("floor");
        assert!((a[2] - 36.0).abs() < 0.1);
        // Under a real low ceiling, the lifted probes start in solid and the
        // level one is the only thing that finds the floor. crouch_corridor's
        // lintel spans 0..32 and its hull-1 tree is solid from z = 12 up, so a
        // probe from 36 + 44 is inside it.
        let c = crouch_corridor();
        assert!(
            drop_to_floor(&c, 16.0, 0.0, 36.0 + MAX_JUMP, MAX_JUMP + MAX_FALL).is_none(),
            "the lifted probe should start inside the lintel"
        );
        assert!(
            floor_in_window(&c, -40.0, 0.0, 36.0).is_some(),
            "the open corridor either side must still find its floor"
        );
    }

    /// Every recorded edge on a real map, not just the ones on one route.
    #[test]
    fn every_edge_on_a_real_map_is_re_admitted() {
        let Some(g) = real_grid("cs_assault") else { return };
        let (bsp, grid) = (&g.0, &g.2);
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
                        bsp.hull_trace(Hull::Stand, n.origin, q).is_clear(),
                        "ladder link {i} -> {} passes through solid",
                        l.to
                    );
                } else {
                    assert_eq!(
                        classify(bsp, n.origin, q),
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
        let mut check = |label: &str, p: Vec3| {
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
        let mut counts = [0usize; 5];
        for n in &grid.nodes {
            for l in &n.links {
                counts[l.kind.to_byte() as usize] += 1;
            }
        }
        eprintln!(
            "de_dust2 moves: walk={} jump={} fall={} crouch={} ladder={}",
            counts[0], counts[1], counts[2], counts[3], counts[4]
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
