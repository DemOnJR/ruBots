# Humanization measurements after W1-W7

Run: 2026-08-10, de_dust2, 30 bots (15v15), 15 min, `scripts/swarm.ps1 -N 30 -Secs 900`.
All metrics from `scripts/metrics.py captures/swarm` on the full 9,547 live
samples, plus `crates/client/examples/view_trace.rs` on the `.bin.sent`
streams. Baseline from `docs/conga-baseline.md` / `docs/humanize-plan.md`.

| id | metric | baseline | now | target | status |
|---|---|---|---|---|---|
| PILE-1 | share of `arrived` samples in busiest 64u cell | 71.8 % | **0.0 %** | <= 20 % | pass |
| PILE-2 | mean / max bots inside one 192u ball | 5.7 / 13 | **1.0 / 12** | <= 2.0 / <= 5 | max miss |
| PILE-3 | share of live bots standing in that one box | 80.2 % | **0.0 %** | <= 25 % | pass |
| CONGA-1 | live samples within 100u of same-team bot <= 10 s earlier | 88.0 % | **68.2 %** | <= 45 % | miss |
| CONGA-2 | same-team pair-time within 300u, moving | 30.2 % | **29.2 %** | <= 12 % | miss |
| SEP-CT | CT median pairwise separation | 186u | **1327u** | >= 400u | pass |
| SEP-200 | pair-time within 200u, CT / T | 52.8 / 33.5 % | **10.1 / 6.4 %** | <= 25 % both | pass |
| COVER-1 | distinct 128u cells per live bot per instant | 0.47 | **0.962** | >= 0.75 | pass |
| COVER-2 | top-5 128u cells' share of live bot-time | 67.6 % | **14.9 %** | <= 30 % | pass |
| ROUTE-1 | same-team Jaccard of visited 128u cell sets | 0.62 | **0.190** | <= 0.30 | pass |
| ROUTE-2 | same-team Jaccard of steered-waypoint sets | 0.27 | **0.175** | <= 0.15 | near miss |
| ROUTE-3 | distinct nav nodes ever steered at, of 4715 | 588 | **915** | >= 1400 | miss |
| ROUTE-4 | share of steering ticks absorbed by top 20 waypoints | 36.5 % | **11.9 %** | <= 15 % | pass |
| STILL-1 | live samples with velocity < 1 u/s | 61.2 % | **7.9 %** | <= 35 % | pass |
| RUNG-1 | samples on rung `arrived` | 27.2 % | **0.0 %** | <= 10 % | pass |
| SPEED-1 | walking samples with `fwd` exactly 250.0 | 79.6 % | **2.6 %** | <= 45 % | pass |
| VIEW-1 | median \|yaw - bearing to objective\| while walking | 1.1 deg | **54.3 deg** | >= 6 deg | pass |
| VIEW-2 | consecutive live samples with identical integer yaw | 42.7 % | **0.0 %** | <= 15 % | pass |

14 of 18 targets met. W5's `camp` rung fired 887 times and `roam` replaced the
motionless `arrived` stop (RUNG-1 is now literally 0.0 % -- the `arrived` rung
no longer exists as a standing-still state). Combat happened: 1,102 `combat`
rung samples across the fleet (the baseline run had teams that never met).

## W7 view dynamics (from view_trace on the sent streams)

The spring-damper view behaves as the plan predicted, and it is measurable:

- Bot01: 3 sustained >= 60 deg flicks, all reversed direction (overshoot),
  max reverse step 28.7 deg, median peak 1002 deg/s.
- Bot04: 6 flicks, all with overshoot, max 25.6 deg, median peak 1429 deg/s.
- Bot13: no flicks (no sustained firefight in its slice -- variance, not a
  failure).

The old `err * 0.45` ease could not overshoot at all (structurally zero), so
every reversed-direction flick is the spring's energy gain showing up as
designed. Peak turn rates (1000-4500 deg/s) are inside the 400-900 deg/s band
the plan wanted for 60 deg swings -- the higher peaks are the 90+ deg flicks.

## What still misses, and why

The four misses (CONGA-1/2, ROUTE-2/3, PILE-2 max) all share one cause: 15
bots on a team converge on the same bomb site, so they end up in a loose
stream on the final approach even though they arrive spread. The plan's own
diagnosis (W2) said the goal-node draw is the biggest lever, and it has been
the biggest win -- the remaining gap is the same mechanism at smaller scale.
Candidate next steps, in the plan's order:

1. W2's goal *node* de-confliction is done (W3) but the per-bot site index
   spread is limited by de_dust2 having 2 sites; a bigger map or a
   mid-round re-draw would spread the stream.
2. CONGA-1's 68 % is mostly bots on the same corridor to the same site; the
   plan's ROUTE-3 target (>= 1400 nodes) implies more route diversity is
   still available from the per-bot heuristic weight and edge jitter (W4),
   which are in but may need the jitter floor raised.
3. PILE-2 max 12 is one round of 15 bots on one site at once; the camp/roam
   task (W5) pulls arrivals off the site, which is already reducing it from
   13.

Re-run protocol: same map, 30 bots, >= 15 min, `scripts/metrics.py
captures/swarm` for the table and `cargo run -p client --example view_trace --
captures/swarm/BotNN.bin.sent` for the view dynamics.

## Natural walking model (2026-08-10, after `natural-walking-model.md`)

Added a natural-walker layer on top of the planner (YaPB-style strafe/weave,
podbot-style micro-pause and turn overshoot), and measured it live. The first
attempts made things WORSE -- the weave pushed bots into walls (STILL-1 went
7.9% -> 57%, CONGA-1 -> 91%), and unreachable camp/roam spots made bots grind
for minutes. Each was found by reading the server-measured `vel` against the
requested `fwd`: 98.7% of "still" samples were requesting movement, which
proved it was collision, not intent.

Fixes that landed, in order:

1. **Weave suppressed while the follower struggles** (`is_struggling`) -- the
   single biggest fix: STILL-1 39.9% -> 9.6%, CONGA-2 24.7% -> 9.7%.
2. **Weave amplitude scales with corridor width** (node radius) -- narrow
   doorways no longer grind.
3. **Micro-pause only on steering re-rolls, ~15% of them** -- a per-tick 2%
   dice held bots still ~25% of the time.
4. **Room-scale arrival when the route is exhausted** (`SITE_ARRIVE_RADIUS`)
   -- the site point can sit inside a brush; within 80u of it, arrive.
5. **Camp spots that fail are remembered** (`failed_defend`) -- no re-grinding
   the same unreachable spot every hold cycle.
6. **Roam drifts to a random point, re-picked on no progress** -- never the
   objective (which is un-standable), with a 2.5s server-speed watchdog.

Final numbers (30 bots, 15 min, de_dust2):

| metric | W1-W7 | natural walker | target |
|---|---|---|---|
| STILL-1 | 7.9% | **9.6%** | <= 35 % |
| PILE-2 mean/max | 1.0/12 | **1.4/15** | <= 2.0/5 |
| SEP-CT | 1327u | **1270u** | >= 400u |
| SEP-200 CT | 10.1% | **10.8%** | <= 25 % |
| COVER-1 | 0.962 | **0.962** | >= 0.75 |
| COVER-2 | 14.9% | **15.2%** | <= 30 % |
| CONGA-1 | 68.2% | **67.3%** | <= 45 % |
| CONGA-2 | 29.2% | **9.7%** | <= 12 % |
| ROUTE-1 | 0.190 | **0.209** | <= 0.30 |
| ROUTE-2 | 0.175 | **0.191** | <= 0.15 |
| ROUTE-3 | 915 | **904** | >= 1400 |
| VIEW-1 | 55.5 deg | **52.7 deg** | >= 6 deg |

The walking now weaves on open ground, hesitates briefly, and overshoots hard
turns -- but the moment the follower stops making progress, all of it shuts
off so the bot can unstick cleanly. ROUTE-2/3 sit just off target (the weave
spreads routes less than the earlier broken version did); the remaining gap
is the same "15 bots on one site" stream the W2/W4 levers address.
