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

use nav::navgrid::NavGrid;
use nav::route::NavSource;

/// How close counts as arrived, horizontally.
///
/// Comfortably inside the 40-unit lattice pitch so a slightly overshot
/// waypoint still registers.
pub const ARRIVE_RADIUS: f32 = 32.0;

/// How much closer to the waypoint counts as real progress rather than noise.
///
/// The origin arrives quantised and a bot shuffling against a wall drifts a few
/// units either way; anything under this is not travel.
pub const PROGRESS_EPSILON: f32 = 4.0;

/// How long to make no progress before giving up on the waypoint.
pub const STUCK_SECONDS: f32 = 1.2;

/// Grace before the first nudge.
///
/// A single tick without measurable progress is not being stuck -- the bot is
/// usually still turning to face the waypoint it was handed a moment ago.
/// Nudging on the first one would have every bot permanently strafing.
pub const UNSTICK_AFTER: f32 = 0.35;

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
    /// Waypoints skipped because we could not reach them. Diagnostic: a bot
    /// that constantly re-routes is a graph problem, not a steering one.
    pub reroutes: u32,
    /// How long we have been trying to get unstuck, and which way we are
    /// currently leaning. See [`Unstick`].
    unstick_for: f32,
    unstick_dir: f32,
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

impl PathFollower {
    pub fn new() -> Self {
        Self { unstick_dir: 1.0, best_dist: f32::INFINITY, ..Self::default() }
    }

    pub fn path_len(&self) -> usize {
        self.path.len()
    }

    pub fn remaining(&self) -> usize {
        self.path.len().saturating_sub(self.at)
    }

    /// Abandon the current route; the next [`next_waypoint`](Self::next_waypoint)
    /// will plan afresh.
    pub fn reset(&mut self) {
        self.path.clear();
        self.at = 0;
        self.goal = None;
        self.no_progress_for = 0.0;
        self.unstick_for = 0.0;
    }

    /// Stop judging progress: the bot is deliberately not going there.
    ///
    /// Being stuck is measured as "not getting closer to the waypoint", which
    /// is exactly what a bot looks like when it has broken off to fight. Left
    /// running, a firefight manufactures a stuck verdict, and the escape
    /// behaviour then jumps the bot and swings its view off the target -- so
    /// the navigation layer would be sabotaging every gunfight.
    ///
    /// Re-bases on resume rather than freezing, so the first tick back counts
    /// as progress and the bot is not immediately declared stuck for ground it
    /// lost while fighting.
    pub fn hold(&mut self) {
        self.no_progress_for = 0.0;
        self.unstick_for = 0.0;
        self.best_dist = f32::INFINITY;
    }

    /// Are we currently blocked?
    pub fn is_stuck(&self) -> bool {
        self.unstick_for > UNSTICK_AFTER
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
        Some(if blocked_for < 0.6 {
            Unstick { sidemove: 250.0 * dir, jump: false, yaw_bias: 0.0 }
        } else if blocked_for < 1.4 {
            Unstick { sidemove: 250.0 * dir, jump: true, yaw_bias: 25.0 * dir }
        } else {
            Unstick { sidemove: 180.0 * dir, jump: true, yaw_bias: 60.0 * dir }
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
        // A new destination invalidates the route.
        if self.goal.map(|g| dist2d(g, goal) > ARRIVE_RADIUS).unwrap_or(true) {
            self.replan(grid, from, goal);
        }

        let mut target = self.current_target(grid, from, goal);
        self.watch(target, from);

        match target {
            Some(t) => {
                let d = dist2d(from, t);
                if d + PROGRESS_EPSILON < self.best_dist {
                    self.best_dist = d;
                    self.no_progress_for = 0.0;
                    self.unstick_for = 0.0;
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
        target
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
            let p = grid.origin(self.path[self.at]);
            if dist2d(from, p) <= ARRIVE_RADIUS {
                self.at += 1;
            } else {
                return Some(p);
            }
        }
        if dist2d(from, goal) > ARRIVE_RADIUS {
            Some(goal)
        } else {
            None
        }
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
        self.goal = Some(goal);
        self.at = 0;
        self.tracked = None;
        self.best_dist = f32::INFINITY;
        self.no_progress_for = 0.0;
        self.path = match (grid.nearest(from), grid.nearest(goal)) {
            (Some(a), Some(b)) => grid.find_path(a, b).unwrap_or_default(),
            _ => Vec::new(),
        };
    }

    /// Give up on the current waypoint.
    ///
    /// Skipping one is usually enough — a blocked door or a player standing in
    /// a corridor. If we have run out, plan again from scratch.
    fn advance_past_blocked(&mut self, grid: &NavGrid, from: [f32; 3], goal: [f32; 3]) {
        if self.at + 1 < self.path.len() {
            self.at += 1;
        } else {
            self.replan(grid, from, goal);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dust2() -> Option<crate::map::Map> {
        crate::map::Map::load("de_dust2")
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
        assert!(f.unstick().is_none(), "nudged before it had a chance to walk");

        // Blocked -- the origin never changes, so the distance to the waypoint
        // never comes down. Strafe first, no jump yet.
        for _ in 0..25 {
            f.next_waypoint(&map.grid, start, goal, 0.02);
        }
        let a = f.unstick().expect("blocked bot should be nudged");
        assert!(a.sidemove.abs() > 0.0, "first response is a sidestep");
        assert!(!a.jump);

        // Still blocked: add the jump. Stay under STUCK_SECONDS so this tests
        // the escalation and not the give-up.
        for _ in 0..30 {
            f.next_waypoint(&map.grid, start, goal, 0.02);
        }
        let b = f.unstick().expect("still blocked");
        assert!(b.jump, "a persistent block should provoke a jump");
        assert!(b.yaw_bias.abs() > 0.0, "and stop staring at the wall");
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

        // Give up on the waypoint, which is what flips the side.
        let before = f.reroutes;
        assert!(step(&mut f, &|f| f.reroutes > before), "never gave up");
        assert!(step(&mut f, &|f| f.unstick().is_some()), "never nudged again");

        let second = f.unstick().expect("still blocked").sidemove.signum();
        assert_ne!(first, second, "must try the other side after giving up");
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
