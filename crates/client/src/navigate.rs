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
}

fn dist2d(a: [f32; 3], b: [f32; 3]) -> f32 {
    let (dx, dy) = (a[0] - b[0], a[1] - b[1]);
    (dx * dx + dy * dy).sqrt()
}

impl PathFollower {
    pub fn new() -> Self {
        Self::default()
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
            if self.stuck_for >= STUCK_SECONDS {
                self.stuck_for = 0.0;
                self.reroutes += 1;
                self.advance_past_blocked(grid, from, goal);
            }
        } else {
            self.stuck_for = 0.0;
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
