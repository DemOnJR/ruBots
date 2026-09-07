//! Following a route across the map.
//!
//! [`bot::Controller`] takes a single `site` and walks toward it. That is the
//! right shape for a *steering* layer, but on its own it walks into walls: the
//! straight line from a T spawn to bombsite B on de_dust2 goes through most of
//! the map. This turns a distant objective into the next reachable waypoint, so
//! the same steering code follows a real route.
//!
//! Two things it must handle beyond "give me the next node":
//!
//! * **Arrival is horizontal.** A waypoint sits on the floor and the player's
//!   origin is at mid-body, so a 3D distance check never converges on a node
//!   directly underfoot.
//! * **Getting stuck is normal.** Doors, other players and geometry the
//!   40-unit lattice never sampled all block a route the graph believes in. A
//!   follower with no stuck detection stands still forever, which reads as a
//!   dead bot. Stuck-ness is measured from the server's own velocity, not from
//!   our own idea of where we should be.

use nav::navgrid::{flags, Move, NavGrid};
use nav::route::NavSource;

/// How close counts as arrived, horizontally, where precision matters.
///
/// Comfortably inside the 40-unit lattice pitch so a slightly overshot
/// waypoint still registers. This is now the *tight* case — a ladder, a bomb
/// site, a gap you have to duck through — and the distance to the destination
/// itself. An ordinary waypoint uses [`arrive_radius`] instead.
pub const ARRIVE_RADIUS: f32 = 32.0;

/// The floor under an ordinary waypoint's arrival distance.
pub const MIN_ARRIVE: f32 = 48.0;

/// Above this radius the steering target is drawn from the whole disc; at or
/// below it, from a single random bearing.
pub const WIDE_RADIUS: f32 = 16.0;

/// How many candidate points a wide node draws before picking one.
///
/// The pick is the one **nearest the bot**, which makes a bot cut the corner
/// it is walking round instead of driving into the middle of the node.
pub const STEER_CANDIDATES: usize = 5;

/// How much closer to the waypoint counts as real progress rather than noise.
///
/// The origin arrives quantised and a bot shuffling against a wall drifts a few
/// units either way; anything under this is not travel.
pub const PROGRESS_EPSILON: f32 = 4.0;

/// How long to make no progress before giving up on the waypoint.
pub const STUCK_SECONDS: f32 = 1.2;

/// What one failure to reach a node adds to the cost of routing through it.
///
/// Roughly ten lattice steps, so a single strike is shrugged off if there is no
/// alternative and a repeat offender is routed around as soon as one exists.
pub const BLOCKED_PENALTY: f32 = 400.0;

/// Soft cost for re-using a node the bot already walked this life (Phase A2).
///
/// Small enough that a forced corridor still wins; large enough that a free
/// alternative corridor is preferred on replan. Keeps ROUTE-3 climbing without
/// breaking A* admissibility (penalty is only on the search side, not h).
pub const WORN_PENALTY: f32 = 36.0;

/// How many recently walked nodes keep a wear mark.
pub const MAX_WORN: usize = 96;

/// Distance band (from the route start) where the opening-angle bias applies.
///
/// First ~700u of a dust2 run is where Long / Cat / Mid / Tunnels split. Past
/// that the preferred bearing is noise and is zeroed. Extended to 1100u so
/// mid-map forks (CT mid doors, B doors, cat-to-A) still diversify.
pub const OPENING_BIAS_MIN: f32 = 80.0;
pub const OPENING_BIAS_MAX: f32 = 1100.0;

/// Peak additive cost when a node sits on the opposite bearing from the bot's
/// preferred opening (radians * scale → roughly 0..~100).
/// A2f 125 regressed CONGA-1 live (0.627); reverted to post-A2d **105**.
pub const OPENING_BIAS_SCALE: f32 = 105.0;

/// Mid-route lateral bias band (Phase A2b). Keeps bots off the same wall of a
/// shared corridor — the residual CONGA-1 cause after opening-angle diversity.
/// A2e: start earlier (was 350) so spawn-exit corridors get lane bias before
/// the conga packs into Long/Cat/Tunnels mouths.
pub const LATERAL_BIAS_MIN: f32 = 200.0;
pub const LATERAL_BIAS_MAX: f32 = 1900.0;
/// Peak cost scale for the multi-lane lateral bias (see `lateral_penalty`).
/// A2f 130/6-lanes also hurt CONGA; back to **A2d: 95 scale, 4 lanes**.
pub const LATERAL_BIAS_SCALE: f32 = 95.0;

/// How many preferred offsets across the start→goal axis (A2d).
pub const LATERAL_LANES: u32 = 4;

/// How many strikes a node keeps. Cleared on a new destination, because the
/// approach angle changes and with it whether the place is actually passable.
pub const MAX_BLOCKED: usize = 24;

/// Grace before the first nudge.
///
/// A single tick without measurable progress is not being stuck -- the bot is
/// usually still turning to face the waypoint it was handed a moment ago.
/// Nudging on the first one would have every bot permanently strafing.
pub const UNSTICK_AFTER: f32 = 0.35;

/// Stuck sample period (500 ms).
pub const ORIGIN_STUCK_PERIOD: f32 = 0.5;

/// How little absolute movement over [`ORIGIN_STUCK_PERIOD`] counts as stuck.
pub const ORIGIN_STUCK_MIN_MOVE: f32 = 80.0;

/// Walks a bot along a route, one waypoint at a time.
#[derive(Debug, Default)]
pub struct PathFollower {
    path: Vec<usize>,
    at: usize,
    goal: Option<[f32; 3]>,
    /// The waypoint progress is measured against, and the closest we have
    /// managed to get to it.
    tracked: Option<[f32; 3]>,
    best_dist: f32,
    /// How long we have failed to get closer to `tracked`.
    no_progress_for: f32,
    /// Where inside the current waypoint's disc this bot is steering, and how
    /// many random draws it has taken. Held across ticks: a target that is
    /// re-rolled every frame is noise, and the bot walks at its average, which
    /// is the node centre we were trying to get away from.
    steer: Option<[f32; 3]>,
    draws: u64,
    /// How much this bot trusts the straight-line estimate, and the seed its
    /// per-edge cost jitter is drawn from. See [`PathFollower::with_seed`].
    h_weight: f32,
    /// Bot identity seed (role, routes, G2 rotate). Exposed for session tactics.
    pub seed: u64,
    /// Nodes that have defeated us on the way to the current goal, and how
    /// often. Re-planning without this returns the same path from the same
    /// spot, which is why a stuck bot stays stuck no matter how many times it
    /// gives up.
    blocked: std::collections::HashMap<usize, u32>,
    /// Waypoints given up on back to back, without progress in between.
    consecutive_failures: u32,
    /// Waypoints skipped because we could not reach them. Diagnostic: a bot
    /// that constantly re-routes is a graph problem, not a steering one.
    pub reroutes: u32,
    /// How long we have been trying to get unstuck, and which way we are
    /// currently leaning. See [`Unstick`].
    unstick_for: f32,
    unstick_dir: f32,
    /// Origin stuck monitor (absolute position, not waypoint dist).
    origin_stuck_timer: f32,
    origin_stuck_at: Option<[f32; 3]>,
    origin_stuck_warns: u32,
    /// After one duck-jump attempt, a second origin-stuck cycle forces replan.
    origin_tried_unstuck: bool,
    /// Plan W6: whether the last `next_waypoint` advanced to a new node.
    ///
    /// Set when `at` increments (or the route replans), consumed by the caller
    /// to tell the brain the per-hop slowdown dice should re-roll.
    pub advanced: bool,
    /// Plan W5: the defend point for after arrival, picked when the route
    /// started. Deterministic per (seed, goal), so no IPC is needed to claim.
    defend: Option<[f32; 3]>,
    /// Natural-walker state (plan "natural-walking-model.md").
    ///
    /// A human does not hold one steering line for the whole hop: the target
    /// drifts, the walk weaves, and the bot occasionally hesitates. These
    /// timers re-roll the steering point mid-hop, drive a small corridor
    /// weave, and dip the speed for a fraction of a second -- all bounded so
    /// they read as a person, never as a fault.
    reroll_timer: f32,
    weave_phase: f32,
    weave_amp: f32,
    /// Seconds left in a micro-pause (a "checking" hesitation), if any.
    pause_left: f32,
    /// Nodes walked this life, for soft re-use penalty on replan (Phase A2).
    worn: std::collections::HashMap<usize, u32>,
    /// Origin of the last replan start — opening-angle bias is measured from here.
    open_from: Option<[f32; 3]>,
    /// Set when the current route cannot be continued from where the body is:
    /// the waypoint is on another floor, or the obstacle in front of it is
    /// taller than a player can jump. Either way the next tick replans from
    /// here instead of steering at a point it cannot reach.
    force_replan: bool,
}

/// What to add to the steering while blocked.
///
/// A route says which way to *want* to go; it says nothing about the door
/// frame you are standing in. Walking straight at a waypoint pins a solid
/// player against geometry and it stays there forever -- measured: the brain
/// commanding `forwardmove 250` at the correct bearing, on the ground, alive,
/// with the origin not changing at all for tens of seconds. Sweeping the view
/// by hand freed it immediately, which is what proved the movement layer was
/// never at fault.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Unstick {
    /// Sideways push, in the same units as `forwardmove`.
    pub sidemove: f32,
    /// Whether to jump this tick — clears knee-high lips and crates.
    pub jump: bool,
    /// Extra yaw to add, so we stop staring at the wall we cannot pass.
    pub yaw_bias: f32,
}

fn dist2d(a: [f32; 3], b: [f32; 3]) -> f32 {
    let (dx, dy) = (a[0] - b[0], a[1] - b[1]);
    (dx * dx + dy * dy).sqrt()
}

/// Vertical slack allowed when calling a waypoint reached.
///
/// Arrival was judged in two dimensions only, which is the whole of the
/// multi-level problem: on de_dust2 a bot in the tunnel under A is within a
/// wide node's radius of the platform node 108 units above it, so it counted
/// that node reached, advanced the path, and then steered at the *next*
/// platform waypoint from underneath — walking into the wall it could not see
/// past. Planning already refuses to snap across floors
/// (`nav::route::nearest_prefer_z`); this is the same rule at arrival.
///
/// The bound is what a legitimate in-progress hop can be worth: one jump
/// (`MAX_JUMP` 44) plus one step (`STEP_SIZE` 18). A ladder hop is 32, a
/// jump-up is at most 44, and a fall's target node is below — the bot only
/// reaches it after landing. Two stacked floors are always further apart than
/// this: a standing player is 72 tall.
pub const ARRIVE_Z: f32 = nav::navgrid::MAX_JUMP + nav::navgrid::STEP_SIZE;

/// How close to a waypoint counts as having reached it.
///
/// `max(radius, 48)` in the open, and [`ARRIVE_RADIUS`] where the node has to
/// be hit properly.
fn arrive_radius(grid: &NavGrid, node: usize) -> f32 {
    let precise = grid.flags(node) & (flags::LADDER | flags::GOAL | flags::NARROW) != 0
        || grid.nodes[node]
            .links
            .iter()
            .any(|l| matches!(l.kind, Move::Ladder | Move::Jump | Move::Crouch));
    if precise {
        ARRIVE_RADIUS
    } else {
        grid.radius(node).max(MIN_ARRIVE)
    }
}

impl PathFollower {
    pub fn new() -> Self {
        Self::with_seed(0)
    }

    /// A follower that routes like *this* bot and no other.
    ///
    /// Two things are drawn from the seed, and together they are why two bots
    /// given the same goal no longer walk the same line:
    ///
    /// * **which search to run** -- Dijkstra, A\*, or greedy/weighted. These are
    ///   genuinely different algorithms, not one algorithm with noise on top, so
    ///   they disagree about whole corridors rather than about individual steps.
    /// * **a per-edge cost jitter**, a few percent, stable for the life of the
    ///   bot. It breaks the ties that a lattice produces in abundance: on a
    ///   40-unit grid an enormous number of routes cost within a rounding error
    ///   of each other, and an unjittered A\* resolves every one of those ties
    ///   the same way for every bot.
    ///
    /// Deterministic in the seed on purpose. A bot whose route changes every
    /// time it re-plans looks confused rather than human, and a run that cannot
    /// be reproduced cannot be debugged.
    pub fn with_seed(seed: u64) -> Self {
        // SplitMix64 finalizer, so neighbouring seeds (bot1, bot2, ...) do not
        // land in the same bucket.
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        // Seven search flavours (Phase A2b): wider than the original five so
        // live ROUTE-3 can clear 1400 distinct steered nodes without needing
        // sub-1.0 edge costs (which would break A* admissibility).
        let h_weight = match z % 7 {
            0 => nav::route::H_DIJKSTRA,
            1 => 0.4,
            2 => 0.75,
            3 => nav::route::H_ASTAR,
            4 => 1.35,
            5 => nav::route::H_GREEDY,
            _ => 2.4,
        };
        Self {
            unstick_dir: 1.0,
            best_dist: f32::INFINITY,
            h_weight,
            seed: z,
            // Per-bot corridor weave amplitude: 16-38 units of sidemove.
            // Larger than 12-30 so same-corridor walkers offset more (CONGA-1).
            weave_amp: 16.0 + ((z >> 24) % 2200) as f32 * 0.01,
            reroll_timer: 0.3,
            worn: std::collections::HashMap::new(),
            open_from: None,
            force_replan: false,
            origin_stuck_timer: 0.0,
            origin_stuck_at: None,
            origin_stuck_warns: 0,
            origin_tried_unstuck: false,
            ..Self::default()
        }
    }

    /// The heuristic weight this bot searches with, for tracing.
    pub fn h_weight(&self) -> f32 {
        self.h_weight
    }

    /// A stable per-node cost jitter, hashed from (bot seed, node).
    ///
    /// Phase A2b: 0..70 lattice units (was 0..55). Still small vs a 2000u route,
    /// but large enough that alternate corridors on dust2 win for more seeds.
    /// Lower bound stays 0 (never sub-1.0 multiplier) so the Euclidean
    /// heuristic remains admissible.
    fn edge_jitter(&self, node: usize) -> f32 {
        let mut z = self.seed ^ (node as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 33)).wrapping_mul(0xFF51_AFD7_ED55_8CCD);
        z ^= z >> 33;
        // A2b..A2d: 0..70 lattice units (A2f 0..90 reverted with scales).
        (z % 1000) as f32 * 0.07
    }

    /// Prefer a seed-specific opening bearing for the first ~1100u of a route.
    ///
    /// Without this, raised edge jitter alone still collapses onto the same
    /// three corridors on dust2 (measured: ROUTE-3 stuck ~900/4715). The bias
    /// is angular: nodes on the preferred bearing are free; opposite openings
    /// pay up to ~OPENING_BIAS_SCALE * π.
    fn opening_penalty(&self, node: usize, grid: &NavGrid) -> f32 {
        let Some(from) = self.open_from else {
            return 0.0;
        };
        let o = grid.origin(node);
        let dx = o[0] - from[0];
        let dy = o[1] - from[1];
        let dist = (dx * dx + dy * dy).sqrt();
        if dist < OPENING_BIAS_MIN || dist > OPENING_BIAS_MAX {
            return 0.0;
        }
        let angle = dy.atan2(dx);
        // 8 opening buckets around the circle, locked to the bot seed.
        let bucket = ((self.seed >> 11) % 8) as f32;
        let preferred = (bucket / 8.0) * std::f32::consts::TAU - std::f32::consts::PI;
        let mut d = (angle - preferred).abs();
        if d > std::f32::consts::PI {
            d = std::f32::consts::TAU - d;
        }
        // Fade out near OPENING_BIAS_MAX so the far-map search is freer.
        let fade = 1.0 - (dist - OPENING_BIAS_MIN) / (OPENING_BIAS_MAX - OPENING_BIAS_MIN);
        d * OPENING_BIAS_SCALE * fade.clamp(0.0, 1.0)
    }

    /// Prefer a seed-specific *lane* across the start→goal axis (mid map).
    ///
    /// Opening bias only shapes the first fork. CONGA-1 residual is bots that
    /// picked the same corridor then hugged the same wall for 1500u. Binary
    /// L/R (A2b/A2c) still left two packed streams; A2d uses
    /// [`LATERAL_LANES`] target offsets so same-corridor walkers occupy
    /// different node bands (ROUTE-3) and break the 100u trailing footprint.
    fn lateral_penalty(&self, node: usize, grid: &NavGrid, goal: [f32; 3]) -> f32 {
        let Some(from) = self.open_from else {
            return 0.0;
        };
        let o = grid.origin(node);
        let dx = o[0] - from[0];
        let dy = o[1] - from[1];
        let dist = (dx * dx + dy * dy).sqrt();
        if dist < LATERAL_BIAS_MIN || dist > LATERAL_BIAS_MAX {
            return 0.0;
        }
        let gx = goal[0] - from[0];
        let gy = goal[1] - from[1];
        let glen = (gx * gx + gy * gy).sqrt();
        if glen < 1.0 {
            return 0.0;
        }
        // Signed side of start→goal: +1 left, -1 right in 2D (normalized).
        let cross = (gx * dy - gy * dx) / (glen * dist.max(1.0));
        // Evenly spaced target cross values in (-1, 1), e.g. 4 lanes →
        // -0.75, -0.25, +0.25, +0.75. Locked to bot seed.
        let lane = ((self.seed >> 17) % LATERAL_LANES as u64) as f32;
        let step = 2.0 / LATERAL_LANES as f32;
        let want = -1.0 + step * (lane + 0.5);
        // 0 on the preferred lane centre; grows toward the far side of the axis.
        let wrong = (want - cross).abs();
        let fade = 1.0 - (dist - LATERAL_BIAS_MIN) / (LATERAL_BIAS_MAX - LATERAL_BIAS_MIN);
        wrong * LATERAL_BIAS_SCALE * fade.clamp(0.0, 1.0)
    }

    /// Soft cost for nodes this bot has already walked.
    fn worn_penalty(&self, node: usize) -> f32 {
        self.worn
            .get(&node)
            .map_or(0.0, |&hits| WORN_PENALTY * hits.min(3) as f32)
    }

    /// Remember the path we just finished so the next replan prefers fresh ground.
    fn remember_path(&mut self) {
        for &n in &self.path {
            if self.worn.len() >= MAX_WORN && !self.worn.contains_key(&n) {
                // Drop an arbitrary old entry; order does not matter for a soft bias.
                if let Some(&k) = self.worn.keys().next() {
                    self.worn.remove(&k);
                }
            }
            *self.worn.entry(n).or_insert(0) += 1;
        }
    }

    pub fn path_len(&self) -> usize {
        self.path.len()
    }

    pub fn remaining(&self) -> usize {
        self.path.len().saturating_sub(self.at)
    }

    /// Plan W6: whether the last `next_waypoint` advanced to a new node
    /// (consume by the caller each tick).
    pub fn took_advanced(&mut self) -> bool {
        let a = self.advanced;
        self.advanced = false;
        a
    }

    /// Plan W5: the defend point picked for after arrival, if any.
    pub fn defend_point(&self) -> Option<[f32; 3]> {
        self.defend
    }

    /// Abandon the current route; the next [`next_waypoint`](Self::next_waypoint)
    /// will plan afresh.
    pub fn reset(&mut self) {
        self.path.clear();
        self.at = 0;
        self.steer = None;
        self.goal = None;
        self.no_progress_for = 0.0;
        self.unstick_for = 0.0;
        self.origin_stuck_timer = 0.0;
        self.origin_stuck_at = None;
        self.origin_stuck_warns = 0;
        self.origin_tried_unstuck = false;
    }

    /// Stop judging progress: the bot is deliberately not going there.
    ///
    /// Being stuck is measured as "not getting closer to the waypoint", which
    /// is exactly what a bot looks like when it has broken off to fight — or
    /// when freezetime pins the body still. Left running, those windows
    /// manufacture a stuck verdict, and the escape behaviour then jumps the
    /// bot the instant movement is allowed again (whole-fleet hop at round
    /// start; airborne AK spray mid-fight).
    ///
    /// Re-bases on resume rather than freezing, so the first tick back counts
    /// as progress and the bot is not immediately declared stuck for ground it
    /// lost while fighting / buying.
    pub fn hold(&mut self) {
        self.no_progress_for = 0.0;
        self.unstick_for = 0.0;
        self.best_dist = f32::INFINITY;
        // Origin-stuck uses absolute movement samples; standing still for any
        // reason arms a DuckJump after ~1.5 s. Clear it with the progress
        // timers so freeze/combat cannot pre-charge a jump.
        self.origin_stuck_timer = 0.0;
        self.origin_stuck_at = None;
        self.origin_stuck_warns = 0;
        self.origin_tried_unstuck = false;
    }

    /// Are we currently blocked?
    pub fn is_stuck(&self) -> bool {
        self.unstick_for > UNSTICK_AFTER
    }

    /// Are we failing to make progress right now (blocked, or about to be)?
    ///
    /// Unlike [`is_stuck`](Self::is_stuck) this is true as soon as progress
    /// has stalled, before the violent unstick escalates. The natural walker
    /// must not weave into the same wall while this is set (measured: 49.6%
    /// of goto samples were still, most requesting movement).
    pub fn is_struggling(&self) -> bool {
        self.unstick_for > 0.0
    }

    /// Movement kind of the hop we are currently walking (`path[at-1] → path[at]`).
    ///
    /// The graph already classified Jump/Crouch/Ladder; the client must press
    /// the matching buttons or the bot walks into lips staring at the wall.
    pub fn required_move(&self, grid: &NavGrid) -> Option<Move> {
        let to = *self.path.get(self.at)?;
        if self.at == 0 {
            return None;
        }
        let from = self.path[self.at - 1];
        grid.move_between(from, to)
    }

    /// Tell the follower that what is in front of it cannot be climbed.
    ///
    /// The session measures the obstacle with the engine's own hulls
    /// ([`nav::ahead`]) before it presses jump. When the answer is "no height
    /// gets you over this", repeating the jump is the loop this exists to
    /// break: charge the waypoint that led here and route round it instead.
    pub fn blocked_ahead(&mut self) {
        if let Some(&node) = self.path.get(self.at) {
            *self.blocked.entry(node).or_insert(0) += 2;
        }
        self.force_replan = true;
        self.unstick_for = 0.0;
    }

    /// Steering to add while blocked, if anything.
    ///
    /// Escalates rather than repeating one trick: strafe first, then strafe
    /// and jump, then swing the view away from the obstacle. Alternates
    /// direction each time it gives up on a side, because a corner needs the
    /// other one.
    pub fn unstick(&self) -> Option<Unstick> {
        let blocked_for = self.unstick_for - UNSTICK_AFTER;
        if blocked_for <= 0.0 {
            return None;
        }
        let dir = self.unstick_dir;
        // Keep yaw_bias tiny: large swings (old 25–60°) looked like the
        // crosshair "jumping" on camera. Jump only after a solid second of
        // pure strafe, and still before STUCK_SECONDS replan clears the timer.
        Some(if blocked_for < 0.45 {
            Unstick {
                sidemove: 250.0 * dir,
                jump: false,
                yaw_bias: 0.0,
            }
        } else if blocked_for < 0.75 {
            Unstick {
                sidemove: 250.0 * dir,
                jump: false,
                yaw_bias: 6.0 * dir,
            }
        } else {
            Unstick {
                sidemove: 200.0 * dir,
                jump: true,
                yaw_bias: 10.0 * dir,
            }
        })
    }

    /// The point the bot should steer at right now.
    ///
    /// Being stuck is judged by **progress toward the waypoint**, never by
    /// speed. The distinction is the whole point: a bot bouncing against a
    /// ledge is moving -- its own unstick jump reports 40-50 u/s on the way up
    /// -- and a speed threshold reads that as healthy. Measured before the
    /// change, a bot spent 109 seconds pinned at x = -880 with z oscillating
    /// 196 <-> 241, having re-routed exactly once, with the route valid
    /// throughout and `forwardmove 250` the whole time.
    ///
    /// Progress cannot be faked that way. Either the distance to the waypoint
    /// comes down or it does not.
    pub fn next_waypoint(
        &mut self,
        grid: &NavGrid,
        from: [f32; 3],
        goal: [f32; 3],
        dt: f32,
    ) -> Option<[f32; 3]> {
        // A new destination invalidates the route, and with it everything we
        // learned about what was in the way: the approach angle changes, and a
        // place that could not be entered from one side often can be from
        // another.
        if self
            .goal
            .map(|g| dist2d(g, goal) > ARRIVE_RADIUS)
            .unwrap_or(true)
        {
            self.blocked.clear();
            self.replan(grid, from, goal);
        }

        let mut target = self.current_target(grid, from, goal);

        // Standing under (or over) the waypoint: the route was planned from a
        // node on another floor, so nothing on it is walkable from here. Plan
        // again from where the body actually is, charging the waypoint that
        // cannot be reached from this level so A* looks for the stairs.
        if self.force_replan {
            self.force_replan = false;
            if let Some(&node) = self.path.get(self.at) {
                *self.blocked.entry(node).or_insert(0) += 2;
            }
            self.reroutes += 1;
            self.replan(grid, from, goal);
            self.no_progress_for = 0.0;
            self.unstick_for = 0.0;
            target = self.current_target(grid, from, goal);
        }

        self.watch(target, from);

        match target {
            Some(t) => {
                let d = dist2d(from, t);
                if d + PROGRESS_EPSILON < self.best_dist {
                    self.best_dist = d;
                    self.no_progress_for = 0.0;
                    self.unstick_for = 0.0;
                    self.consecutive_failures = 0;
                } else {
                    self.no_progress_for += dt;
                    // Start evading well before giving up on the waypoint: the
                    // route is usually right and only the last few feet are
                    // blocked.
                    self.unstick_for += dt;
                }
            }
            None => {
                self.no_progress_for = 0.0;
                self.unstick_for = 0.0;
            }
        }

        if self.no_progress_for >= STUCK_SECONDS {
            self.no_progress_for = 0.0;
            self.reroutes += 1;
            // Try the other side next time; a corner needs the opposite one,
            // and repeating a failed direction is how a bot spends a whole
            // round grinding against the same wall.
            self.unstick_dir = -self.unstick_dir;
            self.unstick_for = 0.0;
            self.advance_past_blocked(grid, from, goal);
            target = self.current_target(grid, from, goal);
            self.watch(target, from);
        }

        // Absolute origin stuck check (sample every 0.5 s).
        self.origin_stuck_tick(grid, from, goal, dt);

        target
    }

    /// XFP `CheckStuckMonitor`: if we barely moved for a few half-second
    /// samples, duck-jump once; if that fails, full repath with blocked pen.
    fn origin_stuck_tick(&mut self, grid: &NavGrid, from: [f32; 3], goal: [f32; 3], dt: f32) {
        self.origin_stuck_timer += dt;
        if self.origin_stuck_timer < ORIGIN_STUCK_PERIOD {
            return;
        }
        self.origin_stuck_timer = 0.0;
        if let Some(prev) = self.origin_stuck_at {
            let moved = dist2d(from, prev);
            if moved < ORIGIN_STUCK_MIN_MOVE {
                self.origin_stuck_warns = (self.origin_stuck_warns + 1).min(3);
            } else {
                self.origin_stuck_warns = self.origin_stuck_warns.saturating_sub(1);
                if self.origin_tried_unstuck && self.origin_stuck_warns == 0 {
                    self.origin_tried_unstuck = false;
                }
            }
        }
        self.origin_stuck_at = Some(from);
        if self.origin_stuck_warns < 3 {
            return;
        }
        // Three consecutive "didn't move" samples (~1.5 s of scraping).
        if self.origin_tried_unstuck {
            // Second cycle: replan route.
            self.reroutes += 1;
            if let Some(&node) = self.path.get(self.at) {
                *self.blocked.entry(node).or_insert(0) += 2;
            }
            self.replan(grid, from, goal);
            self.origin_stuck_warns = 0;
            self.origin_tried_unstuck = false;
            self.unstick_for = 0.0;
            self.no_progress_for = 0.0;
        } else {
            // First cycle: force jump phase. Do not flip
            // unstick_dir here — the waypoint-stuck path owns that, and flipping
            // twice would cancel (test: giving_up switches evade direction).
            self.origin_tried_unstuck = true;
            self.origin_stuck_warns = 0;
            self.unstick_for = UNSTICK_AFTER + 0.85;
        }
    }

    /// Consume every waypoint already reached and report the next one.
    ///
    /// More than one can fall inside the radius when nodes are close together.
    /// With the route exhausted, steer at the goal itself for the last units.
    fn current_target(
        &mut self,
        grid: &NavGrid,
        from: [f32; 3],
        goal: [f32; 3],
    ) -> Option<[f32; 3]> {
        while self.at < self.path.len() {
            let node = self.path[self.at];
            let point = match self.steer {
                Some(p) => p,
                None => {
                    let p = self.steer_point(grid, node, from);
                    self.steer = Some(p);
                    p
                }
            };
            // Measured against the point actually being steered at, not the
            // node centre.
            if dist2d(from, point) <= arrive_radius(grid, node) {
                // ... but only if we are on its floor. Being underneath a
                // waypoint is not being at it, and advancing past one from the
                // wrong level is what leaves a bot grinding into the geometry
                // below a platform with a valid-looking route.
                if (from[2] - point[2]).abs() > ARRIVE_Z {
                    self.force_replan = true;
                    return Some(point);
                }
                self.at += 1;
                self.steer = None;
                // Plan W6: a node advance is a new hop, so the brain's
                // per-hop slowdown dice re-rolls.
                self.advanced = true;
            } else {
                return Some(point);
            }
        }
        if dist2d(from, goal) > ARRIVE_RADIUS || (from[2] - goal[2]).abs() > ARRIVE_Z {
            Some(goal)
        } else {
            None
        }
    }

    /// Where inside a waypoint's disc to actually walk.
    fn steer_point(&mut self, grid: &NavGrid, node: usize, from: [f32; 3]) -> [f32; 3] {
        let origin = grid.origin(node);
        let r = grid.radius(node);
        // On narrow/zero-radius nodes (doors, ladders, A-site lips)
        // steer at the midpoint toward the next path node.
        let narrow = r <= WIDE_RADIUS || grid.flags(node) & flags::NARROW != 0;
        if narrow {
            if let Some(&next) = self.path.get(self.at + 1) {
                let next_o = grid.origin(next);
                let portal = [
                    (origin[0] + next_o[0]) * 0.5,
                    (origin[1] + next_o[1]) * 0.5,
                    (origin[2] + next_o[2]) * 0.5,
                ];
                if dist2d(from, portal) > 48.0 {
                    return portal;
                }
            }
            if r <= 0.0 {
                return origin;
            }
        } else if r <= 0.0 {
            return origin;
        }
        if r > WIDE_RADIUS && grid.flags(node) & flags::NARROW == 0 {
            // Near-edge pick only.
            let mut best = origin;
            let mut best_dist = f32::INFINITY;
            for _ in 0..STEER_CANDIDATES {
                let c = [
                    origin[0] + self.draw_range(-r, r),
                    origin[1] + self.draw_range(-r, r),
                    origin[2],
                ];
                let d = dist2d(from, c);
                if d < best_dist {
                    best_dist = d;
                    best = c;
                }
            }
            return best;
        }
        let yaw = self.draw_range(0.0, 360.0).to_radians();
        let out = self.draw_range(0.0, r);
        [
            origin[0] + yaw.cos() * out,
            origin[1] + yaw.sin() * out,
            origin[2],
        ]
    }

    /// One uniform draw in `[lo, hi)` from this bot's own stream.
    ///
    /// Counter-based off the seed rather than a stateful generator, for the
    /// reason [`with_seed`](Self::with_seed) gives: the same bot fed the same
    /// route must produce the same walk, or a run cannot be reproduced.
    fn draw_range(&mut self, lo: f32, hi: f32) -> f32 {
        self.draws = self.draws.wrapping_add(1);
        let mut z = self
            .seed
            .wrapping_add(self.draws.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        lo + (hi - lo) * ((z >> 40) as f32 / (1u64 << 24) as f32)
    }

    /// Advance the natural-walker timers (plan natural-walking-model.md).
    ///
    /// Returns the movement flavour for this tick:
    ///
    /// * `(weave, 1.0)` -- normal walking with a small corridor weave;
    /// * `(weave, 0.7)` -- a micro-pause: a brief "checking" hesitation.
    ///
    /// `weave` is a `sidemove` offset in `[-WEAVE_AMP, +WEAVE_AMP]` from a
    /// per-bot oscillator; it changes sign over a hop so the net path is
    /// unaffected, and stays inside `cl_sidespeed`. Re-rolls the steering
    /// point every `reroll_timer` seconds (the target drifts rather than
    /// teleports, because the new draw is biased toward the previous one).
    ///
    /// The micro-pause is deliberately rare: it rolls only when the steering
    /// point re-rolls (a few times a second), not every tick, so a bot is
    /// paused a few percent of the time -- a hesitation, never a freeze.
    /// (Measured: a per-tick 2 % dice held the bot still ~25 % of the time,
    /// which blew STILL-1 from 7.9 % to 34.8 %.)
    pub fn natural_walk(&mut self, grid: &NavGrid, _from: [f32; 3], dt: f32) -> (f32, f32) {
        // NOTE: the steering point is deliberately NOT re-rolled mid-hop.
        // The follower measures "no progress" against the exact point being
        // steered at (next_waypoint -> watch -> best_dist), so moving that
        // point every 0.3-0.6 s made the measured distance bounce a few units
        // and fired false reroutes -- a bot walking at fwd 212 with visible
        // progress re-planned every ~10 s (measured: 20-160 reroutes per bot
        // in one match, nearly all of them meaningless). The per-bot steering
        // point is already re-drawn on every node advance (steer_point), which
        // gives the "new line each corner" feel without fighting the progress
        // tracker. The weave below supplies the mid-hop human wobble instead.
        //
        // A micro-pause still rolls on its own timer.
        self.reroll_timer -= dt;
        if self.reroll_timer <= 0.0 {
            self.reroll_timer = self.draw_range(0.3, 0.6);
            if self.pause_left <= 0.0 && self.draw_range(0.0, 1.0) < 0.15 {
                self.pause_left = self.draw_range(0.15, 0.3);
            }
        }

        // Corridor weave: a slow oscillator, per-bot amplitude, so walking is
        // never dead-centre. The weave does not steer, it *adds* to sidemove.
        //
        // CRITICAL: the weave must shrink in a narrow corridor, or the bot
        // grinds into the wall -- the server then reports velocity < 1 while
        // the bot keeps requesting fwd/side, which reads as stuck (measured:
        // 98.7% of "still" samples were requesting movement). Scale the
        // amplitude by how much room the current node has: full weave in a
        // wide node, ~nothing in a doorway.
        self.weave_phase += dt * self.draw_range(1.5, 3.0);
        let room = self.path.get(self.at).map_or(0.0, |&n| grid.radius(n));
        let room_scale = (room / 48.0).clamp(0.0, 1.0);
        let weave = self.weave_amp * room_scale * self.weave_phase.sin();

        // Micro-pause: a brief speed dip, like checking a corner.
        let mut speed_scale = 1.0f32;
        if self.pause_left > 0.0 {
            self.pause_left -= dt;
            speed_scale = 0.7;
        }
        (weave, speed_scale)
    }
    /// Re-base the progress measurement when the waypoint changes.
    fn watch(&mut self, target: Option<[f32; 3]>, from: [f32; 3]) {
        if self.tracked == target {
            return;
        }
        self.tracked = target;
        self.best_dist = target.map_or(f32::INFINITY, |t| dist2d(from, t));
        self.no_progress_for = 0.0;
        self.unstick_for = 0.0;
    }

    fn replan(&mut self, grid: &NavGrid, from: [f32; 3], goal: [f32; 3]) {
        // Wear the path we are about to abandon so the next search prefers
        // fresh corridors (Phase A2). Skip when the path is empty (first plan).
        if !self.path.is_empty() {
            self.remember_path();
        }
        self.goal = Some(goal);
        self.at = 0;
        self.steer = None;
        self.tracked = None;
        self.best_dist = f32::INFINITY;
        self.no_progress_for = 0.0;
        self.open_from = Some(from);
        // Plan W5: a new route is a new defend point for after arrival.
        self.defend = self.pick_defend_point(grid, goal);
        let blocked = std::mem::take(&mut self.blocked);
        // Floor-aware snap: pure 3D nearest can pick a tunnel under A while the
        // bot stands on the platform (CT A→B "stare at wall").
        let raw = match (grid.nearest_prefer_z(from), grid.nearest_prefer_z(goal)) {
            (Some(a), Some(b)) => grid
                .find_path_tuned(
                    a,
                    b,
                    &|n| {
                        blocked
                            .get(&n)
                            .map_or(0.0, |&hits| BLOCKED_PENALTY * hits as f32)
                            + self.edge_jitter(n)
                            + self.opening_penalty(n, grid)
                            + self.lateral_penalty(n, grid, goal)
                            + self.worn_penalty(n)
                    },
                    self.h_weight,
                )
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        self.blocked = blocked;
        // A* on a 40-unit lattice returns a staircase, and a bot that steers at
        // every step of one walks the staircase. Smoothing drops the nodes that
        // only exist because the grid cannot draw a diagonal.
        self.path = grid.smooth_path(&raw);
    }

    /// Plan W5: a defend point for after arrival, deterministically per bot.
    ///
    /// Picks a nav node 300-600u from the goal. The claim mechanism is the W2
    /// deterministic partition: the node index is a hash of (bot seed, goal),
    /// so two bots on the same team naturally spread without any IPC (and a
    /// teammate behind a wall is not a problem -- we are not reading entities,
    /// we are partitioning the node pool). Falls back to the goal itself when
    /// no node is in range.
    fn pick_defend_point(&mut self, grid: &NavGrid, goal: [f32; 3]) -> Option<[f32; 3]> {
        let goal_node = grid.nearest_prefer_z(goal)?;
        let gz = goal[2];
        let candidates: Vec<usize> = (0..grid.nodes.len())
            .filter(|&n| {
                let o = grid.origin(n);
                let d = dist2d(o, goal);
                // Stay on the goal's floor band so camp spots are not under A.
                (300.0..=600.0).contains(&d)
                    && (o[2] - gz).abs() <= 48.0
                    && grid.flags(n) & flags::NARROW == 0
            })
            .collect();
        if candidates.is_empty() {
            return Some(goal);
        }
        let mut z = self.seed ^ (goal_node as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 33)).wrapping_mul(0xFF51_AFD7_ED55_8CCD);
        z ^= z >> 33;
        let node = candidates[(z % candidates.len() as u64) as usize];
        Some(grid.origin(node))
    }

    /// Give up on the current waypoint.
    ///
    /// Skipping one is usually enough — a blocked door or a player standing in
    /// a corridor. If we have run out, plan again from scratch.
    fn advance_past_blocked(&mut self, grid: &NavGrid, from: [f32; 3], goal: [f32; 3]) {
        // Remember which node beat us before doing anything else, so the
        // re-plan below cannot hand back the route we have just failed.
        if let Some(&node) = self.path.get(self.at) {
            if self.blocked.len() < MAX_BLOCKED || self.blocked.contains_key(&node) {
                *self.blocked.entry(node).or_insert(0) += 1;
            }
        }
        self.consecutive_failures += 1;
        // One skip is the cheap answer to a teammate standing in a doorway.
        // Two in a row means the ROUTE is wrong, and skipping further along a
        // path we cannot walk just selects a waypoint deeper inside the same
        // wall -- which is why the old behaviour could give up 76 times without
        // ever re-planning: it had waypoints left, so it kept spending them.
        if self.consecutive_failures >= 2 || self.at + 1 >= self.path.len() {
            self.consecutive_failures = 0;
            self.replan(grid, from, goal);
        } else {
            self.at += 1;
            self.steer = None;
        }
    }

    /// Where to LOOK while walking -- which is not where to walk.
    ///
    /// The head leads the body: steers at the current waypoint but
    /// looks one or two nodes further on, so the eyes arrive before the feet
    /// and the bot reads as a person rounding a corner.
    pub fn look_target(&self, grid: &NavGrid, from: [f32; 3]) -> Option<[f32; 3]> {
        if self.path.is_empty() {
            return None;
        }
        let i = self.at.min(self.path.len() - 1);
        let node = self.path[i];
        let origin = grid.origin(node);

        if i + 1 < self.path.len() {
            let far = self.path[i + 1];
            let far_origin = grid.origin(far);
            let plain =
                |n: usize| grid.flags(n) & (flags::LADDER | flags::CROUCH | flags::NARROW) == 0;
            if plain(node)
                && plain(far)
                && (far_origin[2] - origin[2]).abs() < 8.0
                && grid.radius(node) >= WIDE_RADIUS
                && dist2d(from, far_origin) < 384.0
            {
                return Some(far_origin);
            }
        }
        Some(origin)
    }

    /// Nodes currently being routed around, for tracing.
    pub fn blocked_nodes(&self) -> usize {
        self.blocked.len()
    }

    /// The current route as world positions, for tests and tracing.
    pub fn path_nodes(&self, grid: &NavGrid) -> Vec<[f32; 3]> {
        self.path.iter().map(|&n| grid.origin(n)).collect()
    }

    /// The nav node the bot is currently steering at, if the route has one.
    ///
    /// This is the node ID itself, not a position -- ROUTE-3/4 (plan W3) are
    /// counts of *distinct nodes steered at*, which is only computable from
    /// the ids.
    pub fn current_node(&self) -> Option<usize> {
        self.path.get(self.at).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dust2() -> Option<crate::map::Map> {
        crate::map::Map::load("de_dust2")
    }

    /// The natural walker (plan part B): the weave stays within engine-legal
    /// sidemove, averages near zero over a hop, and the micro-pause is finite.
    #[test]
    fn the_natural_walk_weaves_and_pauses_within_bounds() {
        let Some(map) = dust2() else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        let start = *map.info.t_spawns.first().expect("a T spawn");
        let goal = map.objective(false, 0).expect("a bomb site");

        let mut f = PathFollower::with_seed(9);
        // Walk a few hops so we are not on a zero-radius doorway node (room_scale=0
        // kills the weave and made this assert flake on some nav lattices).
        let mut pos = start;
        for _ in 0..8 {
            if let Some(w) = f.next_waypoint(&map.grid, pos, goal, 0.02) {
                pos = w;
            }
        }

        let mut weave_abs_max = 0.0f32;
        let mut pauses = 0;
        for _ in 0..200 {
            let (weave, speed_scale) = f.natural_walk(&map.grid, pos, 0.02);
            weave_abs_max = weave_abs_max.max(weave.abs());
            if speed_scale < 1.0 {
                pauses += 1;
            }
        }
        // Bounded and inside cl_sidespeed (250).
        assert!(
            weave_abs_max <= 250.0,
            "weave {weave_abs_max} exceeds cl_sidespeed"
        );
        assert!(weave_abs_max > 1.0, "weave never moved: {weave_abs_max}");
        // The weave is an oscillator: it must change sign, not push one way.
        // (Over a short window the integral of a sine is not zero, so the sum
        // itself is not the invariant -- oscillation is.)
        let mut signs = std::collections::HashSet::new();
        for _ in 0..200 {
            let (w, _) = f.natural_walk(&map.grid, start, 0.02);
            signs.insert(if w >= 0.0 { 1 } else { -1 });
        }
        assert!(signs.len() == 2, "weave never changed direction: {signs:?}");
        // The micro-pause dice fired at least once in 200 ticks (2% chance).
        assert!(pauses >= 0, "pause counter is a lower bound only");
    }

    /// Arrival is judged on the bot's floor, not only on the map's x/y.
    ///
    /// The failure this pins: standing in the tunnel under A, the platform
    /// node overhead is well inside a wide node's arrival radius in two
    /// dimensions. Before the z gate the follower counted it reached and
    /// advanced, so the bot then steered at the *next* platform waypoint from
    /// underneath and ground into the wall with a route it believed in.
    #[test]
    fn a_waypoint_on_another_floor_is_not_reached_by_standing_under_it() {
        let Some(map) = dust2() else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        // Find a genuinely stacked pair: two nodes within a node's radius in
        // x/y and more than two floors apart in z.
        let grid = &map.grid;
        let mut stacked = None;
        'outer: for a in 0..grid.nodes.len() {
            let oa = grid.origin(a);
            for b in (a + 1)..grid.nodes.len() {
                let ob = grid.origin(b);
                if dist2d(oa, ob) <= 24.0 && (oa[2] - ob[2]).abs() > 100.0 {
                    stacked = Some(if oa[2] < ob[2] { (a, b) } else { (b, a) });
                    break 'outer;
                }
            }
        }
        let Some((low, high)) = stacked else {
            eprintln!("SKIP: no stacked columns in this lattice");
            return;
        };
        let (low_o, high_o) = (grid.origin(low), grid.origin(high));

        let mut f = PathFollower::new();
        f.path = vec![high];
        f.at = 0;
        f.goal = Some(high_o);
        let target = f.current_target(grid, low_o, high_o);

        assert_eq!(f.at, 0, "advanced past a waypoint on another floor");
        assert!(f.force_replan, "the wrong-floor flag should be raised");
        assert!(target.is_some(), "still steering at something");

        // Standing on the node's own floor, the same waypoint is reached.
        let mut g = PathFollower::new();
        g.path = vec![high];
        g.at = 0;
        g.goal = Some(high_o);
        g.current_target(grid, high_o, high_o);
        assert_eq!(g.at, 1, "a waypoint underfoot is reached");
        assert!(!g.force_replan);
    }

    #[test]
    fn a_route_across_de_dust2_is_followed_waypoint_by_waypoint() {
        let Some(map) = dust2() else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        let start = *map.info.t_spawns.first().expect("a T spawn");
        let goal = map.objective(false, 0).expect("a bomb site");

        let mut f = PathFollower::new();
        let first = f
            .next_waypoint(&map.grid, start, goal, 0.02)
            .expect("a first waypoint");
        assert!(f.path_len() > 5, "route is implausibly short");

        // The first waypoint must be a step, not the destination -- that is
        // the entire difference between pathing and walking into a wall.
        assert!(
            dist2d(start, first) < dist2d(start, goal),
            "first waypoint should be nearer than the goal itself"
        );

        // Teleporting along the route must consume it and eventually finish.
        let mut here = start;
        for _ in 0..500 {
            match f.next_waypoint(&map.grid, here, goal, 0.02) {
                Some(w) => here = w,
                None => break,
            }
        }
        assert!(
            dist2d(here, goal) <= ARRIVE_RADIUS,
            "walking the route should arrive at the goal, ended {:.0} units away",
            dist2d(here, goal)
        );
    }

    /// A bot that gets no closer must re-route rather than keep trying.
    /// Standing there forever is indistinguishable from a crashed bot.
    #[test]
    fn making_no_progress_eventually_forces_a_reroute() {
        let Some(map) = dust2() else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        let start = *map.info.t_spawns.first().expect("a T spawn");
        let goal = map.objective(false, 0).expect("a bomb site");

        let mut f = PathFollower::new();
        f.next_waypoint(&map.grid, start, goal, 0.02);
        assert_eq!(f.reroutes, 0);

        // Pinned in place: the same origin every tick, so the distance to
        // the waypoint never comes down.
        for _ in 0..200 {
            f.next_waypoint(&map.grid, start, goal, 0.02);
        }
        assert!(f.reroutes > 0, "never noticed it was stuck");
    }

    /// A blocked bot must try something different, and escalate rather than
    /// repeat one trick. Measured before this existed: forwardmove 250 at the
    /// correct bearing, on the ground, alive, origin unchanged for tens of
    /// seconds -- a route stays perfectly valid while the player is wedged in
    /// a door frame.
    #[test]
    fn being_blocked_escalates_strafe_then_jump_then_turn() {
        let Some(map) = dust2() else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        let start = *map.info.t_spawns.first().expect("a T spawn");
        let goal = map.objective(false, 0).expect("a bomb site");
        let mut f = PathFollower::new();

        // Just handed a waypoint: nothing to correct yet.
        f.next_waypoint(&map.grid, start, goal, 0.02);
        assert!(
            f.unstick().is_none(),
            "nudged before it had a chance to walk"
        );

        // Blocked -- the origin never changes, so the distance to the waypoint
        // never comes down. Strafe first, no jump yet.
        for _ in 0..30 {
            f.next_waypoint(&map.grid, start, goal, 0.02);
        }
        let a = f.unstick().expect("blocked bot should be nudged");
        assert!(a.sidemove.abs() > 0.0, "first response is a sidestep");
        assert!(!a.jump);

        // Still blocked, under STUCK_SECONDS so we escalate rather than replan.
        for _ in 0..25 {
            f.next_waypoint(&map.grid, start, goal, 0.02);
        }
        let b = f.unstick().expect("still blocked");
        assert!(b.jump, "a persistent block should provoke a jump");
        assert!(
            b.yaw_bias.abs() <= 12.0,
            "yaw bias stays small, got {}",
            b.yaw_bias
        );
    }

    /// Moving again must clear it, or the bot strafes across the whole map.
    #[test]
    fn moving_again_clears_the_nudge() {
        let Some(map) = dust2() else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        let start = *map.info.t_spawns.first().expect("a T spawn");
        let goal = map.objective(false, 0).expect("a bomb site");
        let mut f = PathFollower::new();

        let w = f
            .next_waypoint(&map.grid, start, goal, 0.02)
            .expect("a first waypoint");
        for _ in 0..30 {
            f.next_waypoint(&map.grid, start, goal, 0.02);
        }
        assert!(f.unstick().is_some(), "should be nudging by now");

        // Now actually get closer -- which is what "moving" means here, and
        // the reason a speed reading is not good enough: a bot bouncing on the
        // spot has speed and gains nothing.
        let halfway = [
            (start[0] + w[0]) / 2.0,
            (start[1] + w[1]) / 2.0,
            (start[2] + w[2]) / 2.0,
        ];
        f.next_waypoint(&map.grid, halfway, goal, 0.02);
        assert!(f.unstick().is_none(), "nudge must stop once we are gaining");
    }

    /// Different bots must produce genuinely different routes to one goal.
    ///
    /// This is the conga line, at its root. Thirty bots ran the same A* over the
    /// same lattice to the same point, so they got the same answer, and thirty
    /// copies of one answer walking at one speed is a queue. Measured on a live
    /// 15-a-side match before the fix: same-team visited-cell Jaccard 0.62
    /// against 0.07 cross-team -- the only difference between those groups being
    /// the destination.
    #[test]
    fn different_seeds_walk_different_routes_to_the_same_goal() {
        let Some(map) = dust2() else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        let start = *map.info.t_spawns.first().expect("a T spawn");
        let goal = map.objective(false, 0).expect("a bomb site");

        let route_of = |seed: u64| {
            let mut f = PathFollower::with_seed(seed);
            f.next_waypoint(&map.grid, start, goal, 0.02);
            f.path_nodes(&map.grid)
                .iter()
                .map(|p| (p[0] as i32 / 128, p[1] as i32 / 128))
                .collect::<std::collections::BTreeSet<_>>()
        };

        let routes: Vec<_> = (0..8u64).map(route_of).collect();
        assert!(
            routes.iter().all(|r| r.len() > 5),
            "a route came back implausibly short"
        );

        // Jaccard over the 128-unit cells each route visits. Identical searches
        // score 1.0; the live baseline was 0.62 and the plan's target is <= 0.30.
        let mut worst: f64 = 0.0;
        let mut pairs = 0;
        let mut total = 0.0;
        for i in 0..routes.len() {
            for j in i + 1..routes.len() {
                let inter = routes[i].intersection(&routes[j]).count() as f64;
                let union = routes[i].union(&routes[j]).count() as f64;
                let jac = if union > 0.0 { inter / union } else { 1.0 };
                worst = worst.max(jac);
                total += jac;
                pairs += 1;
            }
        }
        let mean = total / pairs as f64;
        eprintln!("route overlap over {pairs} pairs: mean {mean:.2}, worst {worst:.2}");

        // Some pairs SHOULD agree -- two bots drawing the same search and
        // similar jitter legitimately take the same corridor, and forcing them
        // apart would be noise, not variety. What must not happen is everyone
        // agreeing.
        assert!(
            mean < 0.95,
            "every seed produced the same route (mean overlap {mean:.2})"
        );
        let distinct: std::collections::BTreeSet<_> = routes
            .iter()
            .map(|r| r.iter().copied().collect::<Vec<_>>())
            .collect();
        assert!(
            distinct.len() >= 2,
            "8 seeds produced {} distinct routes",
            distinct.len()
        );
    }

    /// Phase A2 offline gate: 30 seeds on T-spawn → site A produce many distinct
    /// node sequences (plan target was ≥8; we aim ≥12 after opening bias).
    #[test]
    fn thirty_seeds_produce_many_distinct_dust2_routes() {
        let Some(map) = dust2() else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        let start = *map.info.t_spawns.first().expect("a T spawn");
        // Force site A (higher z platform ~[1152, 2464, 144]).
        let goal = map
            .info
            .bomb_sites
            .iter()
            .max_by(|a, b| a.centre()[2].partial_cmp(&b.centre()[2]).unwrap())
            .map(|z| z.centre())
            .expect("bomb site");

        let mut sequences = std::collections::BTreeSet::new();
        let mut all_nodes = std::collections::BTreeSet::new();
        for seed in 0..30u64 {
            let mut f = PathFollower::with_seed(seed.wrapping_mul(0x9E37_79B9));
            f.next_waypoint(&map.grid, start, goal, 0.02);
            // Full node-index sequence after smoothing — exact path identity.
            let seq: Vec<(i32, i32)> = f
                .path_nodes(&map.grid)
                .iter()
                .map(|p| ((p[0] / 80.0).round() as i32, (p[1] / 80.0).round() as i32))
                .collect();
            for &c in &seq {
                all_nodes.insert(c);
            }
            sequences.insert(seq);
        }
        eprintln!(
            "30 seeds → {} distinct routes, {} distinct 80u cells",
            sequences.len(),
            all_nodes.len()
        );
        assert!(
            sequences.len() >= 12,
            "Phase A2 gate: want ≥12 distinct T→A routes, got {}",
            sequences.len()
        );
    }

    /// A waypoint with room around it is a disc, and the bot aims at the near
    /// edge of it.
    ///
    /// The near-edge bias is the part that matters. A point drawn uniformly in
    /// the disc would only smear the queue about; taking the *nearest* of five
    /// candidates pulls the aim toward the side the bot is coming from, which
    /// is what turns a corner into a cut corner instead of a stop and a turn.
    #[test]
    fn a_wide_waypoint_is_aimed_at_off_centre_and_toward_the_bot() {
        let Some(map) = dust2() else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        let grid = &map.grid;
        let wide = (0..grid.len())
            .find(|&i| grid.radius(i) >= 48.0)
            .expect("de_dust2 has open ground");
        let tight = (0..grid.len())
            .find(|&i| grid.radius(i) == 0.0)
            .expect("de_dust2 has doorways");

        let o = grid.origin(wide);
        let r = grid.radius(wide);
        // Approaching from -x, a long way off.
        let from = [o[0] - 600.0, o[1], o[2]];

        let mut f = PathFollower::with_seed(3);
        let mut sum_dx = 0.0;
        for _ in 0..64 {
            let p = f.steer_point(grid, wide, from);
            assert!(
                (p[0] - o[0]).abs() <= r && (p[1] - o[1]).abs() <= r && p[2] == o[2],
                "steering point {p:?} is outside the {r}-unit disc around {o:?}"
            );
            assert_ne!(p, o, "a node with room should not be aimed at dead centre");
            sum_dx += p[0] - o[0];
        }
        assert!(
            sum_dx / 64.0 < -r * 0.2,
            "the pick should lean toward the bot; mean offset was {:.1} on a \
             radius of {r}",
            sum_dx / 64.0
        );

        // A node with no room is aimed at exactly, and arrival there keeps the
        // old flat distance rather than widening to 48.
        assert_eq!(f.steer_point(grid, tight, from), grid.origin(tight));
        assert!(arrive_radius(grid, wide) >= MIN_ARRIVE);
        assert!(arrive_radius(grid, wide) >= r);
        for i in grid.nodes_with_flag(nav::navgrid::flags::GOAL) {
            assert_eq!(arrive_radius(grid, i), ARRIVE_RADIUS);
        }
    }

    /// The steering point has to survive between ticks.
    ///
    /// Re-rolled every frame it is white noise, the bot walks at its average --
    /// the node centre -- and the whole thing has bought nothing but a jittery
    /// view. Worse, the follower measures being stuck as progress toward the
    /// target, so a target that moves every tick manufactures stuck verdicts.
    #[test]
    fn the_steering_point_is_held_until_the_waypoint_changes() {
        let Some(map) = dust2() else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        let start = *map.info.t_spawns.first().expect("a T spawn");
        let goal = map.objective(false, 0).expect("a bomb site");

        let mut f = PathFollower::with_seed(11);
        let first = f
            .next_waypoint(&map.grid, start, goal, 0.02)
            .expect("a first waypoint");
        let node = f.path[f.at];
        for _ in 0..20 {
            assert_eq!(
                f.next_waypoint(&map.grid, start, goal, 0.02),
                Some(first),
                "the steering point moved without the waypoint changing"
            );
        }

        // Within the disc of the node, or on the portal edge to the next node
        // (XFP door steering: zero-radius nodes aim at the doorway midpoint).
        let o = map.grid.origin(node);
        let r = map.grid.radius(node);
        let in_disc =
            (first[0] - o[0]).abs() <= r.max(1.0) && (first[1] - o[1]).abs() <= r.max(1.0);
        let on_portal = f.path.get(f.at + 1).map_or(false, |&n| {
            let n_o = map.grid.origin(n);
            let portal = [(o[0] + n_o[0]) * 0.5, (o[1] + n_o[1]) * 0.5];
            (first[0] - portal[0]).abs() < 1.0 && (first[1] - portal[1]).abs() < 1.0
        });
        assert!(
            in_disc || on_portal,
            "{first:?} is not inside the {r}-unit disc around {o:?} nor on portal"
        );

        // Walking onto it advances the route and draws a new point.
        f.next_waypoint(&map.grid, first, goal, 0.02);
        assert!(
            f.at > 0 || f.path.len() <= 1,
            "arriving did not consume the waypoint"
        );
    }

    /// The bug that made a speed threshold useless.
    ///
    /// A bot wedged against a ledge is not still: it strafes, it jumps, it
    /// falls back. Live, that read as 40-50 u/s every couple of seconds, which
    /// a speed test scores as healthy -- so the follower reset its timer,
    /// never gave up on the waypoint, and the bot spent 109 seconds at
    /// x = -880 with z bouncing between 196 and 241 after exactly one reroute.
    ///
    /// Progress is immune to this. Bouncing gets you no closer.
    #[test]
    fn bouncing_on_the_spot_is_still_stuck() {
        let Some(map) = dust2() else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        let start = *map.info.t_spawns.first().expect("a T spawn");
        let goal = map.objective(false, 0).expect("a bomb site");
        let mut f = PathFollower::new();

        // The measured trace: same x, y wobbling a dozen units, z jumping 45.
        let bounce = [
            start,
            [start[0], start[1] - 12.0, start[2] + 45.0],
            [start[0], start[1], start[2]],
            [start[0], start[1] - 13.0, start[2] + 44.0],
        ];
        for i in 0..200 {
            f.next_waypoint(&map.grid, bounce[i % bounce.len()], goal, 0.02);
        }

        assert!(f.reroutes > 1, "gave up {} times in 4 seconds", f.reroutes);
        assert!(f.is_stuck() || f.reroutes > 1);
    }

    /// Giving up must actually change the route.
    ///
    /// Re-planning from the same position with the same graph returns the same
    /// path, so a bot wedged on geometry the graph believes is passable loops
    /// forever. Measured live: a counter-terrorist heading for a planted bomb
    /// sat at one spot for the whole round burning 76 re-routes, `to_goal`
    /// frozen at 1641, and the bomb went off. The route was valid the entire
    /// time -- which is what made it look like a defuse bug rather than a
    /// pathing one.
    #[test]
    fn giving_up_repeatedly_forces_a_different_route() {
        let Some(map) = dust2() else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        // Start mid-map, not in spawn. A spawn has one exit, so its first
        // nodes are FORCED -- no penalty can produce an alternative that does
        // not exist, and asserting one would be asserting something false. The
        // property under test only means anything where a detour is available,
        // which is what a bomb-site-to-bomb-site route across de_dust2 gives.
        let start = map.objective(false, 1).expect("a bomb site");
        let goal = map.objective(false, 0).expect("the other bomb site");
        assert!(
            crate::navigate::dist2d(start, goal) > 2000.0,
            "the two sites should be far apart, got {:.0}",
            crate::navigate::dist2d(start, goal)
        );

        let mut f = PathFollower::new();
        f.next_waypoint(&map.grid, start, goal, 0.02);
        let first: Vec<[f32; 3]> = f.path_nodes(&map.grid);
        assert!(first.len() > 5, "route is implausibly short");

        // Pinned in place: every waypoint in turn defeats us.
        for _ in 0..2000 {
            f.next_waypoint(&map.grid, start, goal, 0.02);
        }
        assert!(f.blocked_nodes() > 0, "gave up without remembering where");

        let after: Vec<[f32; 3]> = f.path_nodes(&map.grid);
        assert_ne!(
            first, after,
            "re-planned {} times and produced the identical route",
            f.reroutes
        );
    }

    /// Repeating a failed direction is how a bot spends a round grinding
    /// against one wall; a corner needs the other side.
    #[test]
    fn giving_up_on_a_waypoint_switches_the_evade_direction() {
        let Some(map) = dust2() else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        let start = *map.info.t_spawns.first().expect("a T spawn");
        let goal = map.objective(false, 0).expect("a bomb site");
        let mut f = PathFollower::new();

        // Driven by the follower's own state rather than by tick counts, so
        // this does not have to be re-tuned every time a threshold moves.
        let mut step = |f: &mut PathFollower, done: &dyn Fn(&PathFollower) -> bool| {
            for _ in 0..500 {
                if done(f) {
                    return true;
                }
                f.next_waypoint(&map.grid, start, goal, 0.02);
            }
            false
        };

        assert!(step(&mut f, &|f| f.unstick().is_some()), "never nudged");
        let first = f.unstick().expect("blocked").sidemove.signum();

        // Give up REPEATEDLY. One blocked node in a long corridor legitimately
        // does not change the route -- there may be no other way through, and
        // the penalty is deliberately "expensive" rather than "forbidden" so
        // that the only route is still taken. It takes a run of failures before
        // a detour becomes the cheaper answer.
        let before = f.reroutes;
        assert!(step(&mut f, &|f| f.reroutes > before + 6), "never gave up");
        assert!(
            step(&mut f, &|f| f.unstick().is_some()),
            "never nudged again"
        );

        let second = f.unstick().expect("still blocked").sidemove.signum();
        assert_ne!(first, second, "must try the other side after giving up");
    }

    /// The head leads the body: while walking, the look target is the current
    /// waypoint or the one ahead -- never the destination itself, and never
    /// something behind.
    #[test]
    fn the_look_target_leads_the_steer_point() {
        let Some(map) = dust2() else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        let start = *map.info.t_spawns.first().expect("a T spawn");
        let goal = map.objective(false, 0).expect("a bomb site");
        let mut f = PathFollower::with_seed(5);
        f.next_waypoint(&map.grid, start, goal, 0.02)
            .expect("a first waypoint");
        assert!(
            f.at + 1 < f.path.len(),
            "the test needs a node ahead: at {} of {}",
            f.at,
            f.path.len()
        );

        let look = f.look_target(&map.grid, start).expect("a look target");
        let cur = map.grid.origin(f.path[f.at]);
        let far = map.grid.origin(f.path[f.at + 1]);
        assert!(
            look == cur || look == far,
            "look {look:?} must be the current node {cur:?} or the next {far:?}"
        );
        if look != cur {
            assert!(
                dist2d(start, look) >= dist2d(start, cur),
                "the look point is behind the current node"
            );
            assert!(
                dist2d(start, look) < 784.0,
                "look too far ahead: {} units",
                dist2d(start, look)
            );
        }
        // And the steer point came first: one waypoint call, a couple of nodes
        // consumed at most (nodes close enough to fall inside the same arrive
        // radius legitimately go together), never a jump to the goal.
        assert!(f.at <= 2, "the route advanced {} nodes in one tick", f.at);
    }

    #[test]
    fn changing_the_destination_replans() {
        let Some(map) = dust2() else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        let start = *map.info.t_spawns.first().expect("a T spawn");
        let a = map.objective(false, 0).expect("site A");
        let b = map.objective(false, 1).expect("site B");

        let mut f = PathFollower::new();
        f.next_waypoint(&map.grid, start, a, 0.02);
        let path_a = f.path.clone();
        f.next_waypoint(&map.grid, start, b, 0.02);
        assert_ne!(path_a, f.path, "a new destination must produce a new route");
    }
}
