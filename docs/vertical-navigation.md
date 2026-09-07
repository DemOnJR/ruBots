# Vertical navigation: floors, and knowing what you are jumping at

**Symptoms reported:** bots grind on the spot trying to jump obstacles that are
too tall for them, and they do not handle maps where one walkable floor sits
above another.

Both are runtime problems. The **graph** was never wrong about height: nodes
are stacked per column (`Z_MERGE`, `navgrid.rs`) and a `Jump` link is only
emitted where the rise is inside `mp_jump_height`
(`MAX_JUMP = 44`, `classify_with`). What the graph knew, the body did not use.

## 1. Arrival was two-dimensional

`PathFollower::current_target` consumed a waypoint when

```rust
dist2d(from, point) <= arrive_radius(grid, node)
```

with no test on `z` at all. A wide node's arrival radius reaches 96 units, so a
bot standing in the tunnel *under* the A platform was inside the radius of the
platform node 108 units above it. It counted that waypoint reached, advanced,
and then steered at the *next* platform waypoint from underneath — walking into
the wall below the ledge with a route it believed in the whole time.

Planning already refused this snap: `nav::route::nearest_prefer_z` exists
precisely because "pure 3D nearest can snap a bot on A platform to a tunnel
node under mid". Arrival had no equivalent.

**Fix.** A waypoint counts as reached only on its own floor:

```rust
pub const ARRIVE_Z: f32 = MAX_JUMP + STEP_SIZE;   // 62
```

That bound is what a legitimate in-progress hop can be worth — a jump-up is at
most 44, a ladder step is 32, and a fall's target is below and only reached
after landing. Two stacked floors are always further apart than 62, because a
standing player is 72 tall.

Standing on the wrong floor now sets `force_replan`, so the next tick charges
that waypoint and plans again **from where the body actually is** rather than
steering at a point through a ceiling.

Test: `navigate::tests::a_waypoint_on_another_floor_is_not_reached_by_standing_under_it`
finds a genuinely stacked column pair in the real de_dust2 lattice and asserts
the follower does not advance across it, and does advance when standing on the
node's own floor.

## 2. The unstick jump was a timer

`PathFollower::unstick` escalates strafe → strafe+yaw → **jump**, after about
0.75 s of being blocked, with no idea what is in front. Against anything taller
than a player can climb that is an infinite loop: hop, land in the same place,
hop again, while the route stays "valid".

Measured on the live docker server before the fix, four bots on de_dust2 for
200 s: one bot spent roughly 50 s inside a 150-unit box near B with its reroute
counter climbing past 30 (`captures/baseline/bot04.log`).

**Fix.** `nav::ahead` measures the obstacle with the same engine hulls the grid
is built from. It lifts a standing hull in 2-unit steps and asks, at each
height, whether the bot can rise straight up (that is the head-clearance check —
a lip under a low ceiling is not jumpable) and *then* move forward. The first
height that satisfies both is what a jump has to buy:

| verdict | meaning | what the bot does |
| --- | --- | --- |
| `Clear` | nothing within 40 units | walk |
| `Step { rise }` | `rise <= 18` (`sv_stepsize`) | walk; the engine steps it |
| `Jump { rise }` | `rise <= 44` (`mp_jump_height` − margin) | `+jump` |
| `DuckJump { rise }` | up to `44 + 18` | `+jump` **and** `+duck` |
| `Blocked` | no height clears it | stop jumping, charge the waypoint, route round |

The duck bonus is not a magic number: ducking in the air pulls the feet up by
exactly the difference between the two hulls' origin-to-feet distances,
`Hull::Stand.eye_to_feet() - Hull::Duck.eye_to_feet()`.

`Session::obstacle_ahead` caches the sweep — a body wedged in one place gets the
same answer every tick — re-probing after 16 units of movement, 20° of turn, or
250 ms.

When the answer is `Blocked`, the session tells the follower
(`PathFollower::blocked_ahead`), which charges the current waypoint and forces a
replan, and counts the refusal in `Session::refused_jumps`. **A bot with a
climbing `refused_jumps` is being routed into geometry it cannot pass** — that
is a graph problem to chase in `navgrid`, not a steering one, and the counter is
the tell.

## Measured

Four bots (same names, same CD keys, so the same seeded routes), de_dust2 on the
local docker test server, one 200-second run each side. `scripts/metrics.py`:

| metric | before | after | meaning |
| --- | --- | --- | --- |
| **STILL-1** | 0.254 | **0.125** | share of live samples under 1 u/s — time spent not moving |
| CONGA-1 | 0.570 | 0.463 | walking in a teammate's ten-second-old footsteps |
| COVER-2 | 0.343 | 0.297 | share of bot-time in the top five 128u cells |
| live samples | 335 | 296 | |

Read it for what it is: four bots, one run each side, with different enemy
contacts — not a controlled thirty-bot experiment. The halving of STILL-1 is a
real signal and the direction of every other metric agrees with it, but the
number to quote after a proper 30-bot run is the one from that run.

## How to tell if it comes back

* `cargo test -p nav --lib ahead` and `cargo test -p client --lib navigate` —
  the offline gates. Both need `testserver/cstrike/maps/de_dust2.bsp` (they skip
  loudly without it).
* Live: `python scripts/metrics.py captures/<run>` and watch **STILL-1** (share
  of live samples under 1 u/s). A bot pinned against a wall is counted there.
* Live: `grep -c "ahead: nothing clears it" captures/<run>/*.log`. Zero means no
  bot ever met an unclimbable obstacle while stuck. A large number on one bot
  means the route keeps sending it at the same wall.

## Not fixed here

* **Drop-down hesitation.** Falls are still classified by the graph and are not
  reconsidered at runtime; a bot on a ledge with a long drop still re-evaluates
  before committing.
* **Jump *trajectory*.** The probe answers "can I get onto that height", not
  "will I land on the far side of that gap". A running jump across a gap is
  still only attempted where the graph put a `Jump` link.
