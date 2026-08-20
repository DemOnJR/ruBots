# Humanization plan: killing the conga line

The goal is preventing predictable moves in lines one behind another. This execution plan is ordered by visible improvement per unit of effort, with concrete metrics attached to every item.

Everything here is expressible as `usercmd_t` (viewangles, forwardmove, sidemove, buttons) plus console commands.

**Team tactics (sites, lanes, rotation, post-plant, defuse)** are a separate layer — see **`docs/tactics-plan.md` (Phase G)**.

---

## 1. The measured baseline

Corpus: `captures/swarm/bot{1..30}.log`, one 18-minute de_dust2 match, 30 bots (29 with data), telemetry every 2 s, 15,696 samples.

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

---

## 2. The core principle

**All 30 bots compute the same deterministic function of (map, team), so they produce the same answer, and 30 copies of one answer walking at one speed is a conga line.**

It is one shared destination and one deterministic route to it:
- shared seed / difficulty across processes;
- identical objective targets;
- identical route weights;
- flat arrival radius snapping exactly to nodes.

The consequence for the plan: **human variance is different inputs per bot, not noise added to a shared output.** Give each bot its own goal, its own cost function and its own idea of where a waypoint is, and the lines dissolve naturally.

---

## 3. Work items

### W1. Give every bot its own seed and its own difficulty
- Seed taken from identity (`REB_SEED` / `REB_KEY`).
- Difficulty spread drawn per bot uniformly from {Easy, Normal, Hard}.
- Personality profiles: aggression and fear drawn per bot from its seed.

### W2. Per-bot goal node, drawn from a pool and de-conflicted
- Sample candidate nav nodes near site volume.
- Reject previous goal, recent history, and occupied candidate nodes.
- Deterministic partition by bot index or team bus claims.

### W3. Node radius, destination jitter, radius-based arrival
- Compute node radius offline at grid-build time based on clearance sweep.
- Steer at near-edge points inside wide nodes for natural corner cutting.
- Scale arrival distance to node radius.
- A* post-smoothing (greedy skip across straight unobstructed spans).

### W4. Route diversity: per-bot heuristic weight and round-start edge jitter
- Cost function styles: A*, Dijkstra, greedy A* assigned per personality/morale.
- Per-edge cost jitter drawn from bot seed to introduce route variation.

### W5. Something to do after arriving
- Post-arrival camp and roam tasks.
- Defend node selection, sweeping look angles, and periodic re-evaluation.

### W6. Speed variance and natural movement
- Per-waypoint speed variation, cruising pace scaling, and approach slowdown.
- Slight lateral weave in open corridors.

### W7. View dynamics: spring-damper integrator and look-ahead
- Replace exponential ease with critically damped / underdamped angular spring model.
- Guard large turn swings from crossing behind player direction of travel.
- Look-ahead targeting 1-2 nodes forward along the path.
- Per-bot de-synchronised anti-idle drift.

### W8. Chat and radio
- Contextual radio commands and team coordination.

---

## 4. Architectural Rules

- Direct engine velocity writes or private server-only calls cannot be used by a network-only client. Everything must act through legal `usercmd_t` inputs (`forwardmove`, `sidemove`, `buttons`, `viewangles`).
- Always smooth view angles across ticks; never snap instantaneous angles.
- Keep reaction latency human-like to match the selected difficulty level.
