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

/// Below this speed while trying to move, we are stuck on something.
/// A walking player does 220-250 u/s.
pub const STUCK_SPEED: f32 = 20.0;

/// How long to tolerate that before re-routing.
pub const STUCK_SECONDS: f32 = 1.2;

/// Walks a bot along a route, one waypoint at a time.
#[derive(Debug, Default)]
pub struct PathFollower {
    path: Vec<usize>,
    at: usize,
    goal: Option<[f32; 3]>,
    stuck_for: f32,
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
        Self { unstick_dir: 1.0, ..Self::default() }
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
        self.stuck_for = 0.0;
        self.unstick_for = 0.0;
    }

    /// Are we currently blocked?
    pub fn is_stuck(&self) -> bool {
        self.unstick_for > 0.0
    }

    /// Steering to add while blocked, if anything.
    ///
    /// Escalates rather than repeating one trick: strafe first, then strafe
    /// and jump, then swing the view away from the obstacle. Alternates
    /// direction each time it gives up on a side, because a corner needs the
    /// other one.
    pub fn unstick(&self) -> Option<Unstick> {
        if self.unstick_for <= 0.0 {
            return None;
        }
        let dir = self.unstick_dir;
        Some(if self.unstick_for < 0.6 {
            Unstick { sidemove: 250.0 * dir, jump: false, yaw_bias: 0.0 }
        } else if self.unstick_for < 1.4 {
            Unstick { sidemove: 250.0 * dir, jump: true, yaw_bias: 25.0 * dir }
        } else {
            Unstick { sidemove: 180.0 * dir, jump: true, yaw_bias: 60.0 * dir }
        })
    }

    /// The point the bot should steer at right now.
    ///
    /// `speed` is the server's reported horizontal speed, used only to notice
    /// that we are not actually going anywhere.
    pub fn next_waypoint(
        &mut self,
        grid: &NavGrid,
        from: [f32; 3],
        goal: [f32; 3],
        speed: f32,
        dt: f32,
    ) -> Option<[f32; 3]> {
        // A new destination invalidates the route.
        if self.goal.map(|g| dist2d(g, goal) > ARRIVE_RADIUS).unwrap_or(true) {
            self.replan(grid, from, goal);
        }

        // Not moving while we believe we should be: the route is blocked by
        // something the graph does not model. Re-plan from where we actually
        // are rather than insisting on a waypoint we cannot reach.
        if speed < STUCK_SPEED {
            self.stuck_for += dt;
            // Start evading well before giving up on the waypoint: the route
            // is usually right and only the last few feet are blocked.
            self.unstick_for += dt;
            if self.stuck_for >= STUCK_SECONDS {
                self.stuck_for = 0.0;
                self.reroutes += 1;
                // Try the other side next time; a corner needs the opposite
                // one, and repeating a failed direction is how a bot spends a
                // whole round grinding against the same wall.
                self.unstick_dir = -self.unstick_dir;
                self.unstick_for = 0.0;
                self.advance_past_blocked(grid, from, goal);
            }
        } else {
            self.stuck_for = 0.0;
            self.unstick_for = 0.0;
        }

        // Consume every waypoint we have already reached. More than one can
        // fall inside the radius when nodes are close together.
        while self.at < self.path.len() {
            let p = Some(grid.origin(self.path[self.at]))?;
            if dist2d(from, p) <= ARRIVE_RADIUS {
                self.at += 1;
            } else {
                return Some(p);
            }
        }

        // Route exhausted: steer at the goal itself for the last few units.
        if dist2d(from, goal) > ARRIVE_RADIUS {
            Some(goal)
        } else {
            None
        }
    }

    fn replan(&mut self, grid: &NavGrid, from: [f32; 3], goal: [f32; 3]) {
        self.goal = Some(goal);
        self.at = 0;
        self.stuck_for = 0.0;
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
            .next_waypoint(&map.grid, start, goal, 250.0, 0.02)
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
            match f.next_waypoint(&map.grid, here, goal, 250.0, 0.02) {
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

    /// A bot that stops moving must re-route rather than stand there. Standing
    /// still forever is indistinguishable from a crashed bot.
    #[test]
    fn standing_still_eventually_forces_a_reroute() {
        let Some(map) = dust2() else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        let start = *map.info.t_spawns.first().expect("a T spawn");
        let goal = map.objective(false, 0).expect("a bomb site");

        let mut f = PathFollower::new();
        f.next_waypoint(&map.grid, start, goal, 250.0, 0.02);
        assert_eq!(f.reroutes, 0);

        // Pinned in place at zero speed.
        for _ in 0..200 {
            f.next_waypoint(&map.grid, start, goal, 0.0, 0.02);
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

        // Moving: nothing to correct.
        f.next_waypoint(&map.grid, start, goal, 250.0, 0.02);
        assert!(f.unstick().is_none(), "a moving bot must not be nudged");

        // Blocked: strafe first, no jump yet.
        for _ in 0..15 {
            f.next_waypoint(&map.grid, start, goal, 0.0, 0.02);
        }
        let a = f.unstick().expect("blocked bot should be nudged");
        assert!(a.sidemove.abs() > 0.0, "first response is a sidestep");
        assert!(!a.jump);

        // Still blocked: add the jump.
        for _ in 0..40 {
            f.next_waypoint(&map.grid, start, goal, 0.0, 0.02);
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

        for _ in 0..20 {
            f.next_waypoint(&map.grid, start, goal, 0.0, 0.02);
        }
        assert!(f.unstick().is_some());
        f.next_waypoint(&map.grid, start, goal, 250.0, 0.02);
        assert!(f.unstick().is_none(), "nudge must stop once we are moving");
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

        for _ in 0..15 {
            f.next_waypoint(&map.grid, start, goal, 0.0, 0.02);
        }
        let first = f.unstick().expect("blocked").sidemove.signum();
        // Drive it past the reroute threshold, which flips the side.
        for _ in 0..80 {
            f.next_waypoint(&map.grid, start, goal, 0.0, 0.02);
        }
        for _ in 0..15 {
            f.next_waypoint(&map.grid, start, goal, 0.0, 0.02);
        }
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
        f.next_waypoint(&map.grid, start, a, 250.0, 0.02);
        let path_a = f.path.clone();
        f.next_waypoint(&map.grid, start, b, 250.0, 0.02);
        assert_ne!(path_a, f.path, "a new destination must produce a new route");
    }
}
