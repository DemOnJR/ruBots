# Humanization plan: killing the conga line

The complaint is "predictible moves in lines one behind another". Four studies are done
(YaPB movement, YaPB view/combat, YaPB personality, and an audit of our own swarm logs).
This is the execution plan: ordered by visible improvement per unit of effort, with a
number attached to every item so "did it work" is a measurement and not an opinion.

Everything here is expressible as `usercmd_t` (viewangles, forwardmove, sidemove, buttons)
plus console commands. Where YaPB does something a network-only client cannot, it is in
section 5 and not in the work list.

---

## 1. The measured baseline

Corpus: `captures/swarm/bot{1..30}.log`, one 18-minute de_dust2 match, 30 bots (29 with
data), telemetry every 2 s, 15,696 samples. Every number below is from those logs and is
re-derivable (section 6). Do not start work until you can reproduce this table.

| id | metric | baseline | target after the plan |
|---|---|---|---|
| PILE-1 | share of `arrived` samples in the single busiest 64u cell | **71.8 %** | <= 20 % |
| PILE-2 | mean / max bots inside one 192u ball | **5.7 / 13** | <= 2.0 / <= 5 |
| PILE-3 | share of all live bots standing in that one box | **80.2 %** | <= 25 % |
| CONGA-1 | live samples within 100u of where a same-team bot stood <= 10 s earlier | **88.0 %** (63.0 % on walking rungs) | <= 45 % / <= 35 % |
| CONGA-2 | same-team pair-time within 300u, live and moving (cross-team: 3.6 %) | **30.2 %** | <= 12 % |
| SEP-CT | median same-team pairwise separation, CT side | **186u** (T side 441u) | >= 400u |
| SEP-200 | pair-time within 200u, CT / T | **52.8 % / 33.5 %** | <= 25 % both |
| COVER-1 | distinct 128u cells occupied per live bot per instant (1.00 = everyone alone) | **0.47** | >= 0.75 |
| COVER-2 | top-5 128u cells' share of live bot-time | **67.6 %** | <= 30 % |
| ROUTE-1 | same-team Jaccard of visited 128u cell sets (cross-team 0.07) | **0.62** | <= 0.30 |
| ROUTE-2 | same-team Jaccard of steered-waypoint sets (cross-team 0.05) | **0.27** | <= 0.15 |
| ROUTE-3 | distinct nav nodes ever steered at, of 4715 | **588 (12.5 %)** | >= 1400 |
| ROUTE-4 | share of steering ticks absorbed by the top 20 waypoints | **36.5 %** | <= 15 % |
| STILL-1 | live samples with velocity < 1 u/s | **61.2 %** | <= 35 % |
| RUNG-1 | samples on rung `arrived` | **27.2 %** | <= 10 % |
| SPEED-1 | walking samples with `fwd` exactly 250.0 | **79.6 %** | <= 45 % |
| VIEW-1 | median \|yaw - bearing to objective\| while walking (p90) | **1.1 deg (7.9)** | >= 6 deg (>= 25) |
| VIEW-2 | consecutive live samples with an identical integer yaw | **42.7 %** | <= 15 % |

Health caveats on the baseline run, so nobody over-reads it: 10 of 30 bots never reached
the objective; cross-team proximity <= 300u was 1.1-6.0 % of live samples, so the two
teams essentially never met and the 611 combat samples are too thin to support any claim
about fighting. Re-run the baseline on a match where the teams actually collide before
judging any combat/view work (W6).

---

## 2. The one thing

**All 30 bots compute the same deterministic function of (map, team), so they produce the
same answer, and 30 copies of one answer walking at one speed is a conga line.**

It is one shared destination and one deterministic route to it, four times over:

- one seed for the whole fleet: `crates/client/examples/capture_running.rs:156`
  `bot::Controller::new(0xA1F0, bot::Difficulty::Normal)` -- literal seed, literal
  difficulty, identical in every process;
- one objective: `capture_running.rs:159` `session.load_map(0)` and `:207`
  `session.refresh_objective(0)` reach `crates/client/src/map.rs:73`
  `v[seed % v.len()].centre()`, so with `seed = 0` both teams get bomb site index 0. All
  29 logs print the identical line `objective: [-1536 2688 48]`;
- one route: `crates/nav/src/route.rs:107-164` is a plain A* with `h` = straight-line
  distance (`route.rs:128`), no jitter, no per-bot cost bias, no tie-break randomisation,
  and `nearest()` (`route.rs:167-173`) is a bare `min_by` with `Ordering::Equal` on ties,
  so two bots a few units apart snap to the same start node;
- one arrival: `crates/client/src/navigate.rs:27` `ARRIVE_RADIUS = 32.0` flat, so the
  followed path lies *on* the 40-unit lattice rather than anywhere near it.

The evidence that this and not steering is the cause: same-team visited-cell Jaccard 0.62
against cross-team 0.07 (the only difference between the two groups is the destination),
and 588 of 4715 nav nodes ever used.

The consequence for the plan: **human variance is different inputs per bot, not noise
added to a shared output.** Do not try to fix this by adding jitter to `forwardmove` or
wobble to the view. Give each bot its own goal, its own cost function and its own idea of
where a waypoint is, and the lines dissolve on their own. Work items W1-W4 are that; W5-W7
are the polish that stops each individual bot reading as a machine once they are no longer
in a queue.

---

## 3. Work items, in order

Each item lists: what changes, the YaPB algorithm and citation, the file in our tree, the
metric that proves it, and the effort. Merge them in this order -- W1 is a prerequisite for
W2/W4/W6, and W3 is a prerequisite for the offline test in W4.

### W1. Give every bot its own seed and its own difficulty

**Effort: ~1 hour. Visible improvement on its own: near zero -- and say so.** This is an
enabler: nothing downstream currently reads the RNG for routing, so W1 alone changes
nothing a spectator sees. It is first because W2, W4 and W6 are all one-line changes *once
it exists* and are un-implementable without it.

- Change: `capture_running.rs:156` takes the seed from the bot identity
  (`AIPLAYERS_SEED`, defaulting to a hash of `AIPLAYERS_KEY`) and the difficulty from
  `AIPLAYERS_DIFFICULTY`, defaulting to a draw from the seed. `scripts/swarm.sh` passes
  both. Difficulty spread: draw uniformly from {Easy, Normal, Hard} per bot
  (`crates/bot/src/task.rs:75-80`), not one global level.
- YaPB: difficulty is drawn per bot at construction, `rg(3,4)` by default or
  `rg(cv_difficulty_min, cv_difficulty_max)` -- `yapb/src/manager.cpp:175-182`, `1210-1221`.
  Personality is a second, orthogonal axis: 50 % Normal, else 50/50 Rusher/Careful, each
  carrying `m_baseAgressionLevel` / `m_baseFearLevel` drawn from a personality window
  (`manager.cpp:199-213`, `1236-1254`): Rusher `rg(0.7,1.0)` / `rg(0.0,0.4)`, Careful
  `rg(0.2,0.5)` / `rg(0.7,1.0)`, Normal `rg(0.4,0.7)` / `rg(0.4,0.7)`.
- Ours: `crates/client/examples/capture_running.rs:156`, `scripts/swarm.sh:41-49`, plus two
  `f32` fields (`aggression`, `fear`) on `bot::Controller` drawn in `new()` from
  `crates/bot/src/rng.rs`.
- **How we know it worked:** a config assertion, not a behaviour metric -- 30 logs, 30
  distinct seeds, >= 3 distinct difficulties. **Red flag: this item has no behavioural
  measure.** If it stays un-consumed by W2/W4/W6 it has bought nothing; do not count it as
  progress.

### W2. Per-bot goal node, drawn from a pool and de-conflicted

**Effort: ~1 day. This is the single largest visible win.** It is what turns the pile into
a spread and, because the goals differ, it is most of what breaks the walking line too.

- Change: replace "the site centre" with "a goal *node*". `map.rs:68` keeps returning the
  site volume; a new step samples 4 candidate nav nodes from the nodes whose origin is
  inside or within ~400u of that volume, rejects any that is (a) this bot's previous goal,
  (b) in this bot's last-3 goal history, (c) claimed by a teammate, then takes one. A
  Rusher takes `candidates[rg(0,3)]` without ranking. Re-drawn every round, not once at
  join.
  - Sub-part, 10 minutes: also make the *site index* per-bot -- `map.rs:73`
    `v[seed % v.len()]` with the W1 seed. Be honest about the ceiling: de_dust2 has 2 bomb
    sites, so this alone splits 30 bots into 2 piles of 15. The goal-node draw is the part
    that matters.
- YaPB: `postProcessGoals` draws 4 goals at random from the chosen class and rejects
  `m_prevGoalIndex`, `m_previousNodes[0]`, anything in `m_goalHist`, anything already
  chosen, and any `isOccupiedNode(index, true)`, retrying up to `max(4, poolSize)` times
  (`yapb/src/navigate.cpp:268-383`; Rusher bypass at `353-360`, sort at `361-376`; filter
  at `385-440`). Occupancy: `isOccupiedNode` is true when a living same-team client within
  320u of *me* is within `clamp(radius^2 * 2, 98^2, 120^2)` of the candidate, or when
  another bot's `m_currentNodeIndex` / `m_previousNodes[0]` equals it
  (`yapb/src/navigate.cpp:3299-3334`).
- Ours: `crates/client/src/map.rs:68-83`, `crates/client/src/session.rs:1532-1539`
  (`refresh_objective`), `crates/bot/src/objective.rs`.
- **Process-topology constraint, read before implementing.** `scripts/swarm.sh` spawns 30
  separate OS processes, and a single CS client only receives entities inside its PVS
  (`crates/client/src/view.rs:300-333` builds `PlayerView` from the entity frame), so a bot
  genuinely cannot see a teammate 300u away through a wall. Three options, in the order to
  take them:
  1. **Deterministic partition by bot index** -- no IPC at all. Sort the candidate pool by
     node id and have bot *i* take `pool[(hash(i) + round) % pool.len()]`. Buys most of the
     spread for zero infrastructure. Do this first.
  2. Localhost UDP claim bus: each bot broadcasts `(bot_id, node_id, expiry)` on a fixed
     port, TTL 5 s. ~80 lines, gets the exact YaPB rule.
  3. Eventually: one process hosting N `Session`s, at which point the claim table is a
     `HashMap` and `isOccupiedNode` ports verbatim.
  Degrading to "teammates I can currently see" is the fallback and it is measurably worse;
  do not pretend otherwise in the code comments.
- **How we know it worked:** PILE-1 <= 20 % (from 71.8), PILE-2 max <= 5 (from 13),
  PILE-3 <= 25 % (from 80.2), SEP-CT median >= 400u (from 186), SEP-200 CT <= 25 % (from
  52.8). Also: >= 6 distinct `objective:` lines across 30 logs.

### W3. Node radius, destination jitter, radius-based arrival

**Effort: ~1-2 days (the radius sweep is the bulk). Largest win on the *walking* line.**
YaPB never steers at a node centre and never counts a node as reached at a fixed distance,
which is why its bots occupy a corridor as a bundle rather than a queue.

- Change, three parts:
  1. **Radius per node, computed offline at grid-build time.** For `scan = 32..112 step
     16`, walk 18 directions (`0..360 step 20 deg`); at each, hull-trace at
     `origin + forward*scan`, floor-drop-trace `scan+60` downward on both the `+forward`
     and `-forward` sides, and a head-clearance hull trace to `+34 z`. On the first failure
     `radius -= 16` and break both loops; if a door was hit, `radius = 0`. After the loop
     `radius -= 16` again and clamp to `>= 0`. Result is one of {0,16,32,48,64,80,96}.
     Force `radius = 0` for ladder/goal/camp/crouch nodes and for any node linked to a
     ladder node.
  2. **Steer at a random point inside the disc.** If `radius > 16` and not narrow: generate
     5 candidates `origin + (rg(-r,r), rg(-r,r), 0)` and take the one *nearest the bot's
     current position* -- that near-edge selection is what produces corner-cutting. If
     `radius` is in `(0,16]`: `path_origin += forward(pitch, wrap(yaw + rg(-90,+90))) *
     rg(0.0, radius)`. Recompute on every node advance.
  3. **Arrival distance from the radius.** `desired_dist_sq = max(radius^2, 48^2)` in the
     normal case; 25 when ducking or on a goal node; 6 on a ladder or crouch node; 0 when
     the current node has any travel-flagged link; 48 for 0.5 s after a re-plan.
- YaPB: radius sweep `yapb/src/graph.cpp:1451-1545`; `Bot::setPathOrigin`
  `yapb/src/navigate.cpp:2629-2679` (called from `advanceMovement:2623` and
  `updateNavigation:1090`); arrival ladder `yapb/src/navigate.cpp:1327-1386`.
- Ours: `crates/nav/src/navgrid.rs:198` add `pub radius: f32` to `NavNode`; compute it with
  the existing `NavGrid::trace` / `trace_ignoring_breakables`
  (`navgrid.rs:370-380`); **bump `VERSION` at `navgrid.rs:1048` from 1 to 2** and extend the
  serializer at `1052` -- an old cache will otherwise deserialize wrong rather than error.
  Consumers: `crates/client/src/navigate.rs:27` (`ARRIVE_RADIUS`) and
  `PathFollower::next_waypoint`; `crates/bot/src/controller.rs:631` uses the jittered point
  as `steer`.
- Free companion (30 minutes, same PR): **A\* post-smoothing.** Greedy skip over the
  returned path -- keep the last emitted node `s`, and emit `path[i]` only when
  `cant_skip(s, path[i+1])`. `cant_skip(a,b)` is true if either radius is 0, or they are
  not mutually visible, or `|a.z - b.z| > 17`, or either is flagged `NARROW`
  (`crates/nav/src/navgrid.rs:37-45` already re-exports YaPB's flag set), or
  `dist_sq > 400^2`, or either has a jump link. This is exactly the fix for a 40-unit
  lattice's zig-zag. YaPB: `yapb/src/planner.cpp:222-240`, predicate at `176-220`.
  **Do not copy the `tooClose` test at `planner.cpp` -- it reads
  `distanceSq < cr::sqrtf(40.0f)` (~6.32), a typo for `sqrf`, and is dead code.**
- **How we know it worked:** ROUTE-3 >= 1400 distinct nodes steered (from 588), ROUTE-4
  <= 15 % (from 36.5), CONGA-1 on walking rungs <= 35 % (from 63.0), ROUTE-1 <= 0.30 (from
  0.62). Plus a unit test in `crates/nav`: for a fixed start/goal, the smoothed path has
  <= 60 % of the raw path's node count and no node pair further apart than 400u.

### W4. Route diversity: per-bot heuristic weight and round-start edge jitter

**Effort: ~3 hours. High value, and it is testable offline without a server.**

- Change, two parts:
  1. **Three cost functions, assigned per bot per round.** We have no danger table (see
     section 5), so use the portable half: vary the heuristic weight in
     `route.rs:128`. `h * 1.0` = A*, `h * 0.0` = Dijkstra, `h * 1.6` = weighted/greedy.
     Pick per bot per round from `morale = if fear > aggression { chance(0.30) } else
     { chance(0.70) }` and the W1 personality: Normal -> `morale ? A* : greedy`, Rusher ->
     `morale ? greedy : A*`, Careful -> `morale ? A* : Dijkstra`. Two bots on the same team
     with the same goal are then not even running the same search.
  2. **Per-edge cost jitter, drawn from a per-bot per-round seed.** `find_path_avoiding`
     already takes `penalty: &dyn Fn(usize) -> f32` (`route.rs:107-112`) -- reuse it:
     `penalty(n) = edge_len * (jitter(bot_seed, round, n) - 1.0)` with
     `jitter in [1.00, 1.40]`. **Keep the lower bound at 1.0.** `navgrid.rs:1634` asserts
     `cost_multiplier >= 1.0` "would break A* admissibility"; a sub-1.0 jitter would make
     `h` inadmissible and silently invalidate that test's contract. An additive-only,
     >= 1.0 jitter produces different route *shapes* while leaving the invariant intact.
- YaPB: `resetPathSearchType` picks one of three (g,h) pairs per bot per round by
  personality x a morale coin-flip (`yapb/src/manager.cpp:1766-1790`; the three modes at
  `yapb/src/navigate.cpp:3493-3522`). Round-start scalar: `rsRandomizer = rg(0.5, botTeam
  * 2.0)` applied to every expanded edge's g for the first 2 s of a round
  (`yapb/src/planner.cpp:14, 263-269, 321`). **Port the intent, not the literal** --
  `Team::Terrorist == 0` (`yapb/inc/constant.h:119`), so for T that expression degenerates
  to a reversed range. It is a bug, not a design.
- Ours: `crates/nav/src/route.rs:107-164` (heuristic weight as a parameter),
  `crates/client/src/navigate.rs:42` (`BLOCKED_PENALTY` already composes through the same
  hook), `crates/client/src/session.rs:1379-1391`.
- **How we know it worked:** the cheap one first -- an offline test in `crates/nav`: for
  the fixed de_dust2 T-spawn -> site-A pair, 30 seeds must produce **>= 8 distinct node
  sequences** (baseline: 1). Then from the swarm logs, ROUTE-2 <= 0.15 (from 0.27) and
  COVER-2 <= 30 % (from 67.6).

### W5. Something to do after arriving

**Effort: ~1-2 days. Fixes the second-most-visible number in the whole audit.**
`crates/bot/src/controller.rs:635-639` is the entire post-arrival behaviour:
`if arrived { (0.0, 0.0) }`. That is 27.2 % of the match spent as a motionless body, and
61.2 % of all live samples at velocity < 1 u/s.

- Change: on arrival, enter a task instead of stopping. `crates/bot/src/task.rs:14-33`
  already declares `Guard`/`Roam`/`Camp` and **nothing in the crate ever constructs one**.
  Minimum viable version: pick a defend node 300-600u from the goal that no teammate has
  claimed (reusing W2's claim mechanism), walk to it, hold for
  `rg(camp_min, camp_max)` seconds scaled by the bot's `fear`, sweep the view between two
  world points during the hold, then re-pick. Bomb-site CTs should rotate between two
  angles rather than stand on the plant spot.
- YaPB: camp task with the occupancy veto at `yapb/src/tasks.cpp:147-150`; task selection
  at `yapb/src/botlib.cpp:3906-3910`. The goal class itself is an argmax over four noisy
  scores -- `goalDesire = rg(0,100) + aggression*100`, `campDesire = rg(0,100) +
  fear*100` (zeroed for a non-camp weapon, `* rg(1.5,2.5)` for a sniper), highest wins
  (`yapb/src/navigate.cpp:103-201`). The `rg(0,100)` term is +-100 of noise against a bias
  of at most ~135, which is why the same bot in the same state picks differently round to
  round.
- Ours: `crates/bot/src/controller.rs:629-639`, `crates/bot/src/task.rs:14-33`.
- **How we know it worked:** STILL-1 <= 35 % (from 61.2), RUNG-1 (`arrived`) <= 10 % (from
  27.2, the difference showing up on new `camp`/`roam` rungs in the `obj: rung` telemetry
  line), PILE-2 max <= 5.

### W6. One speed becomes several

**Effort: ~2 hours.** On walking rungs `forwardmove == 250.0` exactly in 79.6 % of samples
and 245-250 in 95.5 %; 30 identical-speed bodies read as a train regardless of route.

- Change: `crates/bot/src/controller.rs:638` passes `FORWARD_SPEED` (`:58`, 250.0)
  unconditionally. Replace with a per-waypoint speed: on each node advance, a
  `25 * difficulty` percent chance to travel that one hop at `0.4 * maxspeed`, plus a
  per-bot baseline scale in `[0.92, 1.00]` so no two bots cruise at the same number, plus
  `WALK_SPEED` (`controller.rs:~70`) when within ~500u of the goal or when the bot's `fear`
  is high.
- YaPB: the per-node speed dice roll is described in the movement study
  (`yapb/src/navigate.cpp`, node-advance path around `2623`); the strafe/jump
  suboptimality is the same family -- 30 % chance to strafe *away* from the correct side
  and a 30 % random jump mid-fight.
- Ours: `crates/bot/src/controller.rs:58, 638`.
- Companion, 20 minutes: lateral movement currently exists **only** inside the combat rung
  (`strafe_side` at `controller.rs:~309` is called from exactly one site, `:452`). Let the
  goto rung add a small `sidemove` when the corridor is wide (radius from W3 >= 48), so
  bots weave instead of tracking dead-centre.
- **How we know it worked:** SPEED-1 <= 45 % (from 79.6), and >= 20 % of moving samples in
  the 100-200 u/s band (baseline: 29.4 % of moving time is in 240-260 u/s and almost
  nothing below 200).

### W7. The view: spring-damper integrator, and stop staring at your feet

**Effort: ~1 day, and it needs new instrumentation first (M0, section 6).** Lowest in the
order not because it is unimportant but because at 30 bots seen from across a map the route
tells dominate, and because at a 2 s sample interval **we currently cannot measure it at
all** -- that is a red flag on the item, not on the technique.

- Change, three parts:
  1. **Replace the exponential ease with a spring.** `crates/bot/src/aim.rs:67-84`
     `turn_toward` is `step = err * 0.45` clamped to 20 deg/tick: monotone, so it can never
     overshoot, never ring, and always decelerates in the same geometric curve regardless of
     distance. Replace with per-axis velocity state, integrated with **explicit Euler**:
     ```
     e     = norm_angle(desired - current)
     accel = clamp(k*e - c*v, -a_max, +a_max)
     v    += dt * accel          // persists across ticks
     angle += dt * v
     ```
     Navigation gains `k=200, c=25, a_max=3000`; combat gains `k=300, c=20, a_max=3300`;
     pitch uses `2k` with the same `c` and `a_max`. `dt = clamp(now - last, eps, 1/25)`.
     At 30 Hz this gives: nav yaw 90 deg -> peak 444 deg/s, 0 overshoot, settled in 0.53 s;
     combat yaw 90 deg -> peak 550 deg/s, **17.2 deg overshoot**, settled in 0.50 s; combat
     pitch 90 deg -> peak 660 deg/s, 42 deg overshoot. The acceleration clamp saturates
     above `e = a_max/k` (15 deg nav yaw, 10 deg combat yaw), so small corrections are a
     pure spring and big swings are bang-bang -- that two-regime shape is why small
     corrections look precise and big swings look thrown. **Do not "fix" the overshoot with
     RK4 or a semi-implicit step: at combat-pitch gains `omega*dt = 0.82`, and Euler's
     energy gain is a large part of the observed overshoot. The numerical artifact is the
     feature.**
  2. **Asymmetric deadband.** Yaw snaps and zeroes its velocity inside 1 degree; pitch
     never does and its velocity is never zeroed. The permanently-excited underdamped pitch
     axis (zeta 0.41-0.63), driven by the eye height bobbing as the bot walks, *is* YaPB's
     breathing motion. There is no injected noise anywhere.
  3. **Look ahead, not down.** `crates/bot/src/controller.rs:632` is
     `turn_toward(view, aim_angles(origin, steer), max_turn)` -- the bot stares at exactly
     the point its feet are walking to, which is VIEW-1's 1.1 deg median. Replace with
     YaPB's ladder: look **two** path nodes ahead when `path_len > 2`, both nodes are
     unflagged, `|next.z - path.z| < 8`, current radius >= 16, not narrow, and
     `dist_sq(origin, dest) < 384^2`; else one node ahead; else the destination. Add the
     "never swing through your own back" guard: when moving and not aiming at an enemy, if
     the current and wanted yaw straddle the direction of travel and the short way round
     goes behind the head, add +-360 to force the long way.
- YaPB: integrator `yapb/src/vision.cpp:124-218` (formula `156-166`, branches `197-212`);
  gain switch `vision.cpp:143-166`; back-swing guard `vision.cpp:172-195`; look-target
  ladder `setAimDirection` `vision.cpp:365-640`; think rate 30 Hz
  `yapb/src/manager.cpp:1750-1763`; `kViewFrameUpdate = 1/25` at
  `yapb/inc/constant.h:448`.
- Ours: `crates/bot/src/aim.rs:67-84` and its `TURN_FACTOR`/`DEFAULT_MAX_TURN` constants
  (`:57, :60`), `crates/bot/src/controller.rs:632`, `crates/client/src/navigate.rs`
  (expose the next-two nodes, not just `steer()`).
- Companion, 20 minutes: **the anti-idle ramp is identical on every bot.**
  `crates/bot/src/idle.rs` builds one `Ramp { amplitude, period }` and every bot sweeps the
  same sawtooth in phase. Derive amplitude, period and starting phase from the W1 seed.
  Keep `AntiIdle::guarantees` satisfied -- `period > 5` and both the advance and the
  wrapped advance above 0.1 deg -- or bots get kicked (`dlls/player.cpp:4779-4783`).
- **How we know it worked:** requires M0 (per-tick trace). From the sent-command stream:
  peak `|dyaw|/s` during a >= 60 deg flick in the 400-900 deg/s band; **overshoot present**
  -- max excursion past the target between 8 and 45 deg on >= 50 % of combat flicks
  (baseline: exactly 0, structurally impossible with a 0.45 ease). From the swarm logs:
  VIEW-1 median >= 6 deg with p90 >= 25 deg (from 1.1 / 7.9), VIEW-2 <= 15 % (from 42.7),
  and distinct anti-idle `(amplitude, period, phase)` tuples = 30.

### W8. Chat and radio (optional, do last, keep it small)

Fully portable: `say` / `say_team` through `crates/client/src/console.rs`, and radio as
`radio1` + `menuselect N`. YaPB even injects typos. **This has no behavioural metric** --
it is taste, not a measurable improvement, and it is listed only so it does not get
smuggled into an earlier item. Ship it after W1-W6 measure clean.

---

## 4. Dependency order

```
W1 (seed/difficulty) --+--> W2 (goal node + de-confliction)   <- biggest win
                       +--> W4 (route diversity)
                       +--> W6 (speed spread)
                       +--> W7 companion (anti-idle phase)
W3 (radius/jitter/arrival) --> W4 offline test, W6 companion (weave)
W2 (claim mechanism)       --> W5 (camp/guard spot claiming)
M0 (per-tick trace)        --> W7
```

Merge W1+W2 together and re-measure before touching anything else. If PILE-1 and SEP-CT do
not move on that pair alone, the model in section 2 is wrong and the rest of the plan needs
revisiting rather than executing.

---

## 5. What NOT to do

**YaPB things a network-only client cannot have.** YaPB is a server plugin with full entity
access and a hand-authored waypoint graph; we are a network client with a BSP-derived
40-unit lattice.

- **Direct `pev->velocity` writes for jumps** (`yapb/src/navigate.cpp:1105`) and
  **`MDLL_Use` on doors**. We press `+jump` / `+use` in the usercmd and accept the engine's
  answer. Any nav edge whose traversal assumed a velocity write must be re-validated as
  button-pressable or dropped from the graph.
- **`isOccupiedNode` in its literal form** (`yapb/src/navigate.cpp:3299-3334`). It reads
  every living same-team client within 320u; a client only receives entities inside its PVS
  (`crates/client/src/view.rs:300-333`), so a teammate behind a wall is invisible to us.
  Use the W2 fallback ladder (deterministic partition -> claim bus -> single process). Do
  not write the code as if it had full entity access and then quietly degrade.
- **The shared danger / practice table.** YaPB's `Optimal` and `Safe` cost functions are
  learned per-team damage scores accumulated across rounds and shared between all bots.
  Reproducing it needs both cross-bot state and many rounds of data. Skipped in W4 on
  purpose; the heuristic-weight variation is the portable half. If it is ever wanted, it is
  a persisted per-map file, and it is the *last* thing to build, not the first.
- **Studio-model hitbox targeting.** YaPB queries bone positions for head/chest aim. We have
  entity origins and angles only (`view.rs:300-333`), so head aim is `origin + fixed
  offset` and will be slightly wrong on animated models. Do not pretend to a precision we
  cannot observe.
- **Voice chatter.** YaPB's voice lines are a fabricated server message. `say` and radio
  are fine; anything that requires *sending* a message a client cannot send is not.
- **Two literal YaPB bugs.** `rg(0.5f, botTeam * 2.0f)` with `Team::Terrorist == 0`
  (`yapb/inc/constant.h:119`) degenerates to a reversed range for T -- port the intent, a
  per-bot per-round draw in `[0.5, 2.0]`. And `cantSkipNode`'s
  `distanceSq < cr::sqrtf(40.0f)` (`yapb/src/planner.cpp`) is a typo for `sqrf` that makes
  the test dead code -- do not reproduce it.

**Things that would look human but are an anticheat / server-detection risk.**

- **Never bypass the view integrator.** YaPB has an explicit aimbot path (Expert + enemy +
  wants-to-fire + a cvar) that writes `pev->v_angle = direction` outright. Writing a
  viewangle with no intermediate frames is the single most recognisable signature in a
  command stream. There is no reason to port it and every reason not to.
- **Never fire on the same tick the view arrives.** Zero-latency acquire-and-shoot is the
  classic detection. Keep the reaction accumulator (charge at 0.3 s per think frame up to
  the difficulty's ideal reaction time, spend to zero on acquisition) so the aim visibly
  converges before the trigger.
- **Do not exceed engine-legal usercmd values.** `forwardmove`/`sidemove` stay within
  `cl_forwardspeed`/`cl_sidespeed`; keep the 250 carry cap as the ceiling
  (`controller.rs:58`). Do not raise the command rate above what the server accepts, and do
  not send `msec` values a real client would not produce. Peak turn rates in W7 (400-900
  deg/s, i.e. <= 30 deg per tick at 30 Hz) are inside what a mouse produces; do not scale
  the gains up "for snappiness" past that.
- **Do not remove the anti-idle drift.** `CheckActivityInGame` (`dlls/API/CSPlayer.cpp:530-540`)
  is an `&&` over both axes sampled 5 s apart, and failing it calls `DropIdlePlayer`, which
  is not gated for a third-party client. W5 will make most bots move anyway; the ramp still
  has to survive for the ones that are camping.
- **Do not add per-tick white-noise view jitter.** It averages to zero over any burst (so it
  costs nothing in accuracy, which is itself the tell), and a flat noise floor on the angle
  deltas is trivially separable from a spring's ringing. The whole point of W7 is that the
  human-looking motion comes from the dynamics, not from an added random term.

---

## 6. Measurement harness

Everything in section 1 comes out of `captures/swarm/bot*.log`. The fields that exist
today, from `crates/client/examples/capture_running.rs:307-360`, sampled every 2 s:

```
  t+ 120s origin [ -1536   2688    48] vel   250 hp 100 maxspeed  250 alive true weapons 4
      brain: alive true frozen false fwd 250 side 0 yaw -37 site Some([-1536,2688]) wp 5 reroutes 12 stuck false
      obj: rung goto       bomb false arming false attack false use false to_goal 812 ...
```

Plus the one-shot `objective: [x y z]` printed at join (`capture_running.rs:~208`).
Team is `bot_index % 2` (odd = T), per `scripts/swarm.sh:38`.

That is enough for every metric in section 1 **except the view dynamics**, which is why:

- **M0 (prerequisite for W7): a per-tick trace.** Either decode the sent-command stream
  already captured to `captures/swarm/Bot<NN>.bin` with the existing
  `crates/client/examples/decode_sent.rs`, or add a one-line-per-tick view log behind an
  env var. Needed fields: `tick, view.yaw, view.pitch, desired.yaw, desired.pitch,
  forwardmove, sidemove, buttons`. Overshoot and peak turn rate are undefined at a 2 s
  sample interval -- any W7 claim made without M0 is unverifiable.
- **A metrics script.** One pass over the logs producing the section-1 table as a
  tab-separated row, so "before" and "after" are two rows and not two recollections. It
  needs no new instrumentation for W1-W6. Store its output next to
  `docs/conga-baseline.md`, which holds the earlier, coarser version of the same
  measurement (CT median separation 186u, 52.8 % within 200u) and should be superseded by
  the section-1 table.
- **Re-run protocol.** Same map (de_dust2), same bot count (30), same duration (>= 15 min),
  same server. Note in the run log whether the teams actually met -- the baseline run had
  cross-team proximity of 1.1-6.0 %, which makes it useless for judging W7 and any other
  combat-facing change.
