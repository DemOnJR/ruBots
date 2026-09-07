//! How high the thing in front of a bot is, and whether jumping can clear it.
//!
//! The grid's edges are already height-checked at build time
//! ([`crate::navgrid::classify`]): a `Jump` link exists only where the rise is
//! inside `mp_jump_height`. That covers moves the router *planned*. It does
//! not cover the other half of a bot's life — being wedged against something
//! the route did not mention (a crate, a doorway lip, a teammate's corner) and
//! reaching for the unstick behaviour.
//!
//! The unstick used to press jump on a timer, with no idea what it was
//! jumping at. Against a 96-unit wall that is an infinite loop: the bot hops,
//! lands in the same place, hops again, and the route stays "valid" the whole
//! time. Measured on a live de_dust2 run, one bot spent ~50 s inside a
//! 150-unit box near B with its reroute counter climbing past 30.
//!
//! This module answers the question the unstick should have been asking:
//! **what is in front of me, how high is it, and is that a height I can get
//! onto?** The answer comes from the same hull traces the grid is built
//! with — the engine's own collision hulls — so it agrees with what the server
//! will actually let the player do.

use crate::bsp::{Hull, Vec3};
use crate::navgrid::{World, MAX_JUMP, STEP_SIZE};

/// How far forward to look. One lattice cell: far enough to see the obstacle
/// the bot is pressed against, near enough that it is not reporting on the
/// next room.
pub const PROBE_REACH: f32 = 40.0;

/// Vertical resolution of the sweep, in units.
const PROBE_STEP: f32 = 2.0;

/// The extra height a duck-jump reaches over a standing jump.
///
/// A jump leaves the ground with enough speed to raise the origin by
/// `mp_jump_height` (45). Ducking in the air pulls the feet up to the ducking
/// hull's floor, and the difference between the two hulls' origin-to-feet
/// distances is exactly how much further up the feet get:
/// `Hull::Stand.eye_to_feet() - Hull::Duck.eye_to_feet()`.
pub fn duck_bonus() -> f32 {
    Hull::Stand.eye_to_feet() - Hull::Duck.eye_to_feet()
}

/// The highest lip a player can get onto at all, feet-to-ledge.
pub fn max_duck_jump() -> f32 {
    MAX_JUMP + duck_bonus()
}

/// What the geometry directly ahead allows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Ahead {
    /// Nothing within [`PROBE_REACH`]: walking forward is not blocked here.
    Clear,
    /// A rise the engine walks up for free (`sv_stepsize`). Do not jump: a
    /// jump over a step is slower than the step, and it spreads the aim.
    Step { rise: f32 },
    /// Clearable with a plain jump.
    Jump { rise: f32 },
    /// Only clearable by ducking at the top of the jump.
    DuckJump { rise: f32 },
    /// Higher than a player can reach, or not a height problem at all (a wall,
    /// a closed door, a corner). Pressing jump will never help here.
    Blocked { probed: f32 },
}

impl Ahead {
    /// Should the bot press `+jump` at this obstacle?
    pub fn wants_jump(self) -> bool {
        matches!(self, Self::Jump { .. } | Self::DuckJump { .. })
    }

    /// Should it also press `+duck` on the way up?
    pub fn wants_duck(self) -> bool {
        matches!(self, Self::DuckJump { .. })
    }

    /// Is this something no amount of jumping will solve — i.e. the reason to
    /// stop trying and route round it?
    pub fn impassable(self) -> bool {
        matches!(self, Self::Blocked { .. })
    }

    /// The measured rise, where there is one.
    pub fn rise(self) -> Option<f32> {
        match self {
            Self::Step { rise } | Self::Jump { rise } | Self::DuckJump { rise } => Some(rise),
            _ => None,
        }
    }
}

/// Measure the obstacle in front of `origin` when facing `yaw` degrees.
///
/// The sweep is the honest version of "can I get up there": lift a standing
/// hull by successive amounts and ask whether it can (a) rise straight up from
/// where the bot is — that is the head-clearance check, a lip under a low
/// ceiling is not jumpable — and then (b) move forward. The first lift that
/// satisfies both is the height the bot must reach, which is exactly what a
/// jump has to buy.
pub fn ahead(world: &World, origin: Vec3, yaw: f32, reach: f32) -> Ahead {
    let (sin, cos) = yaw.to_radians().sin_cos();
    let forward = move |p: Vec3| -> Vec3 { [p[0] + cos * reach, p[1] + sin * reach, p[2]] };

    if world.clear(Hull::Stand, origin, forward(origin)) {
        return Ahead::Clear;
    }

    // `lift` is the height the FEET have to reach — the ledge height. What the
    // origin does to get them there depends on whether the bot is ducking:
    //
    //   standing: feet sit `Stand.eye_to_feet()` below the origin, so the
    //             origin rises by the full ledge height;
    //   ducked:   feet sit `Duck.eye_to_feet()` below it, so the origin only
    //             has to rise by `lift - duck_bonus()` — which is exactly why
    //             a duck-jump reaches higher than a jump on the same impulse.
    //
    // Both legs are asked of the engine's hulls: rise straight up first (the
    // head-clearance check — a lip under a low ceiling is not jumpable), then
    // move across at that height, ducked if that is how the bot would be.
    let limit = max_duck_jump();
    let mut lift = PROBE_STEP;
    while lift <= limit {
        let ducking = lift > MAX_JUMP;
        let origin_rise = if ducking { lift - duck_bonus() } else { lift };
        let up = [origin[0], origin[1], origin[2] + origin_rise];
        let hull = if ducking { Hull::Duck } else { Hull::Stand };
        if world.clear(Hull::Stand, origin, up) && world.clear(hull, up, forward(up)) {
            return if lift <= STEP_SIZE {
                Ahead::Step { rise: lift }
            } else if ducking {
                Ahead::DuckJump { rise: lift }
            } else {
                Ahead::Jump { rise: lift }
            };
        }
        lift += PROBE_STEP;
    }
    Ahead::Blocked { probed: limit }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bsp::Bsp;
    use crate::entities::MapInfo;

    /// Skip cleanly when the map tree is not present (the maps are gitignored;
    /// same convention as the navgrid tests).
    fn dust2() -> Option<(Bsp, MapInfo)> {
        let dir = match std::env::var("AIPLAYERS_MAPS") {
            Ok(d) => std::path::PathBuf::from(d),
            Err(_) => std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("testserver")
                .join("cstrike")
                .join("maps"),
        };
        let bytes = std::fs::read(dir.join("de_dust2.bsp")).ok()?;
        let bsp = Bsp::parse(&bytes).ok()?;
        let info = MapInfo::from_bsp(&bsp).ok()?;
        Some((bsp, info))
    }

    #[test]
    fn the_duck_jump_reach_is_the_jump_plus_the_hull_difference() {
        // Not a magic number: 45 of jump minus a unit of margin, plus the 18
        // the feet gain by ducking.
        assert_eq!(duck_bonus(), Hull::Stand.eye_to_feet() - Hull::Duck.eye_to_feet());
        assert!(max_duck_jump() > MAX_JUMP);
        assert!(max_duck_jump() < 72.0, "a player cannot reach its own height");
    }

    #[test]
    fn open_ground_reads_as_clear_and_a_wall_reads_as_blocked() {
        let Some((bsp, info)) = dust2() else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        let world = World::new(&bsp, &info);
        let spawn = *info.t_spawns.first().expect("a T spawn");

        // At least one of the four cardinal directions out of a spawn is open
        // ground, and a spawn is not sealed in a box.
        let mut sawenough = false;
        for yaw in [0.0, 90.0, 180.0, 270.0] {
            if ahead(&world, spawn, yaw, PROBE_REACH) == Ahead::Clear {
                sawenough = true;
            }
        }
        assert!(sawenough, "no open direction out of the T spawn");
    }

    #[test]
    fn a_verdict_never_asks_for_a_jump_it_cannot_reach() {
        let Some((bsp, info)) = dust2() else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        let world = World::new(&bsp, &info);
        // Sweep a slab of the map and check the invariant on every verdict:
        // the rise reported is always inside what the engine allows, and
        // anything above it is Blocked rather than a jump the bot would keep
        // attempting forever.
        let limit = max_duck_jump();
        let mut seen = 0;
        for x in (-2000..2000).step_by(200) {
            for y in (-2000..2000).step_by(200) {
                let p = [x as f32, y as f32, 64.0];
                for yaw in [0.0, 90.0, 180.0, 270.0] {
                    let verdict = ahead(&world, p, yaw, PROBE_REACH);
                    if let Some(rise) = verdict.rise() {
                        assert!(rise <= limit, "{verdict:?} exceeds the duck-jump reach");
                        assert!(rise > 0.0);
                        seen += 1;
                    }
                    if verdict.wants_duck() {
                        assert!(verdict.rise().unwrap_or(0.0) > MAX_JUMP);
                    }
                }
            }
        }
        assert!(seen > 0, "the sweep never met an obstacle; probe is not working");
    }
}
