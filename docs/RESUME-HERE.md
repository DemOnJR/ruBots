# Where we are, and what to do next

## 2026-08-11: Remote join + fileconsistency (done)

Nexaplay `85.215.153.249:27015`: RevEmu auth OK; bot streams, joins team, moves.
Drop `Reason: Invalid length` / `opcode clc_fileconsistency` was a **netchan
idle leak**, not a consistency bit-pack bug. Documented in
**`docs/remote-join.md`**. Do not “fix” consistency framing first if that
log line returns — decode `.sent` for bare `07` on non-fragment packets.

## 2026-08-11: Loop G2 + A2g (validated live)

**G2 is validated.** `docs/metrics-dust2-2026-08-11-g2-a2g-live.md`:

- **G2-ROTATE 9 / 9 CTs** — 9 distinct rotations, each bot exactly once (dedup + 4 s cooldown hold).
- **Bomb-driven (5):** plant at B pulled Bot02/06/18/20 to site 0 (B), plant at A pulled Bot04 to site 1 (A) — correct site targeting.
- **Pressure-driven (4):** Bot08/10/12/14 rotated on ≥2 visible enemies with no bomb (no false rotates from single contacts).
- **Delay preserved:** delay buckets (seed%10 ∈ {0,3}) held the quiet site under light pressure; Bot16 (B assault) never rotated.
- **A2g non-regressive:** CONGA-1 **0.548** (best since A2d), CONGA-2 0.055, ROUTE-2 0.108, STILL-1 0.195 all in target. Remaining misses are the pre-existing humanization gaps (CONGA-1, ROUTE-3, PILE-2 max 10).

After U2d+A2f CONGA-1 **0.627** miss and Nexaplay 20-bot pre snapshot CONGA-1
**0.579** (`docs/metrics-dust2-2026-08-11-nexaplay-pre-g2.md`):

| Change | Detail |
|---|---|
| **A2g** | Revert A2f crank: `OPENING_BIAS_SCALE` 125→**105**, `LATERAL_BIAS_SCALE` 130→**95**, `LATERAL_LANES` 6→**4**, edge jitter 0..90→**0..70** |
| **G2** | `role::ct_rotate_pick` + `Session::maybe_ct_rotate`: CT repaths when ≥2 visible enemies near one site (or bomb planted); seed buckets 0/3 delay unless ≥3 contacts; 4 s cooldown |
| **Util** | Full/force: **every** bot buys flash; throw machine allows flash for non-owners; smoke/HE stay slot-owned |

Tests: bot **231** ok; client **195** ok (incl. `g2_rotate_fires_on_two_enemies…`).

**Validation 2026-08-11:** the staged 20-bot × 900 s A2g run completed with 5,638
live samples. Metrics: CONGA-1 **0.573** (miss), CONGA-2 **0.051** (pass), ROUTE-2
**0.104** (pass), ROUTE-3 **1121** (miss), STILL-1 **0.208** (pass), VIEW-1
**66.7°** (pass), PILE-2 max **10** (miss). Full report:
`docs/metrics-dust2-2026-08-11-g2-a2g.md`.

The capture format did not expose G2 events, so `Session::apply_ct_rotation` now
logs a cumulative `rotate` counter/site; the parser emits `G2-ROTATE`, and client
library tests are **196** including the deduplication test.

**Instrumented retest (2026-08-11):** resolved — the server was down; after
`docker compose up -d` (fresh container) the 20-bot × 900 s run completed with
5,749 live samples, **G2-ROTATE 9 / 9**, two plants, no mass-drop. See the
validated section above and the full report. G0 can start.

## 2026-08-11: G0 team / round state bus (implemented)

G0 is implemented as a pure reducer plus an optional localhost multicast bus:

- `crates/bot/src/team.rs` — `TeamReport`, `TeamSnapshot`, and `PlantSite`.
  Same-team reports derive alive A/B/mid counts, two-contact site pressure, and
  planted-site belief. A late joiner with only `bomb_planted=true` inherits the
  first teammate's known plant origin.
- `crates/client/src/telemetry.rs` — versioned `APT2` fixed-layout team packet
  and `TeamBus` on multicast `239.255.0.1:27017`; existing radar `APT1` packet is
  unchanged.
- `crates/client/src/session.rs` — local report generation from `WorldView`,
  explicit assigned-site tracking through G1/G2, snapshot reset per round, and
  teammate report ingestion.
- `crates/client/examples/capture_running.rs` — publishes and consumes G0
  reports when `AIPLAYERS_TEAM_PORT` is set.
- `scripts/swarm.ps1` — enables `AIPLAYERS_TEAM_PORT=27017` by default.

Offline G0 tests pass: five CT reports with a B-site wipe derive `pressure=B`,
plant-at-A origin is inherited by a late joiner as `PlantSite::A`, foreign-team
reports are ignored, and the `APT2` packet round-trips with bomb origin, role,
and rung.

Validation: full workspace suite passed, including live sign-on; `capture_running`
built successfully. The two-bot smoke reached signon and joined with the bus
enabled for Bot02; Bot01 was rejected by the server's ReAuthCheck/idle-timeout
state before runtime, so this was not a G0 bind failure. G2 continues to use its
validated local-PVS signal; the next G0 follow-up is to consume `TeamSnapshot`
pressure/plant state in G2 and add snapshot-driven rotation tests.

## 2026-08-11: Upgrade plan + Phase A1 roles (in progress)

Full investigation + research plan: natural walking, aiming, radar GUI, bomb
play. Summary:

- **Freeze de_dust2** until the 18 humanize metrics pass; then other maps/mods.
- **Do not** train MLMove first — use its findings as design constraints
  (wall-hug, cover, team roles). Keep spring aim; add placement later.
- **Next algorithm layers:** roles + approach rings (A1/A3) → route diversity
  (A2) → ORCA-lite local avoid (B) → aim placement (C) → plant spots (D) →
  GUI tabs / APT2 (E) → other maps/mods (F).
- **Phase G — professional team tactics** (planned): CT site/lane defaults,
  rotate on site wipe, post-plant T holds + CT retake/defuse. Full write-up:
  **`docs/tactics-plan.md`**. Can start G0+G1 in parallel with A2 CONGA work.

### Navigation & steering improvements

- **Portal steering** for doorways and narrow gaps.
- Navigation graphs generated from map geometry.

### Shipped this session (Phase A1 / A3)

- `crates/client/src/role.rs` — `BotRole` { Assault, Hold, Flank, Split },
  approach ring destinations 220–450 u from site, plant_spot always in zone,
  carrier override so Hold bots still plant inside the volume.
- `Session::refresh_objective` uses the role picker; `Decision.role` logged.
- Offline tests: role distribution, dust2 site spread, ring fan-out.
- Phase 0 checklist: `docs/metrics-dust2-phase0-checklist.md`.

**Live measure 2026-08-11 (30 bots, 15 min):** see
`docs/metrics-dust2-2026-08-11-roles.md`.

| | prior walker | after roles | target |
|---|---|---|---|
| PILE-2 max | 15 | **6** | ≤5 |
| CONGA-2 | 9.7% | **7.3%** | ≤12% |
| ROUTE-2 | 0.191 | **0.109** | ≤0.15 |
| CONGA-1 | 67% | 71% | ≤45% |
| ROUTE-3 | 904 | 899 | ≥1400 |
| Plants | — | **9** | — |

Roles help pile/route-2; **CONGA-1 + ROUTE-3 need Phase A2 + B next.**
`metrics.py` now accepts `role` on `obj:` lines.

### 2026-08-11 continued: A2 + B + C + D + E (code)

| Phase | Change |
|---|---|
| **A2** | Edge jitter 0..40, 5 h-weights, opening-angle bias first 720u, worn-path soft penalty. Offline: **30/30 distinct** T→A routes. |
| **B** | ORCA-lite sidestep off visible teammates (disabled when struggling); cover bias on steer disc. |
| **C** | Combat aim lead from last-tick enemy origin, difficulty-scaled. |
| **D** | de_dust2 named plant spots (A/B default/open/back). |
| **E** | GUI tabs Radar/Fleet/Settings, heading ticks, zoom, live fleet metrics. |

Live re-measure after this stack is in flight (`swarm.ps1 -N 30 -Secs 900`).

### 2026-08-11 A2b (CONGA-1 / ROUTE-3 push)

Still open after A2 bump: CONGA-1 ~65–68% (≤45%), ROUTE-3 ~1276 (≥1400). PILE-2 max **PASS**.

| Lever | Change |
|---|---|
| Edge jitter | 0..55 → **0..70** |
| Opening bias | max 720→**1100**, scale 85→**105** |
| **Lateral bias** | new: seed left/right of start→goal, 350–1900u, scale 60 |
| h-weights | 5 → **7** flavours (0 / 0.4 / 0.75 / 1 / 1.35 / 1.6 / 2.4) |
| Weave amp | 12–30 → **16–38** |
| Worn penalty | 28 → **36** |
| Role rings | 220–450 → **200–560** |
| XFP stuck | origin sample 0.5s / 80u / jump then repath (prior) |

Offline: **30 seeds → 30 distinct** T→A routes, 266 cells @80u.

**Live:** 20-bot × 900s swarm + GUI; 5-minute measure/improve loop. Avoid MaxDrop mass-kill (docker restart clears ban).

### 2026-08-11 A2c (full measure)

Early snapshot: `docs/metrics-dust2-2026-08-11-a2c-pre.md`.  
Full table: `docs/metrics-dust2-2026-08-11-a2c.md`.

| metric | A2c full (N=20, 6170 live) | target |
|---|---|---|
| CONGA-1 | **0.567** | ≤0.45 |
| CONGA-2 | **0.041** | ≤0.12 pass |
| ROUTE-2 | **0.130** | ≤0.15 pass |
| ROUTE-3 | **1082** | ≥1400 |
| PILE-2 max | **10** | ≤5 |

Scale-only (60→95) moved CONGA-1 a little; binary L/R still packs half the fleet per wall.

### 2026-08-11 A2d (full measure)

Full table: `docs/metrics-dust2-2026-08-11-a2d.md`.

| metric | A2c | **A2d** | target |
|---|---|---|---|
| CONGA-1 | 0.567 | **0.545** | ≤0.45 |
| ROUTE-3 | 1082 | **1222** | ≥1400 |
| ROUTE-2 | 0.130 | **0.150** | ≤0.15 (edge) |
| PILE-2 max | 10 | **10** | ≤5 |

Lanes help ROUTE-3 more than CONGA-1. Next diversity lever: earlier lateral band
or stronger ORCA — not another scale-only bump.

### 2026-08-11 freeze-time mass jump (fixed)

**Symptom:** every bot jumps in sync at freezetime end.  
**Cause:** `next_waypoint` ran during freeze with pinned origin → stuck/unstick
+ origin-stuck armed jump for the whole fleet.  
**Fix:** skip `next_waypoint` while `freeze_period` (same branch as dead);
`hold()` also clears origin-stuck counters. Rebuilt `capture_running`.

### 2026-08-11 Phase G tactics plan + first builds

Full plan: **`docs/tactics-plan.md`**.

| Phase | Status |
|---|---|
| G1 CT default anchors (`role.rs` `dust2_ct_setup`) | **shipped + live measured** — see `docs/metrics-dust2-2026-08-11-g1.md` |
| G4 T post-plant `DefendPlant` + entry holds | **coded + unit tests green**; live swarm after G1 |
| A2e `LATERAL_BIAS_MIN` 350→200 | **coded** with G4 binary |
| G0 team bus / G2 rotate / G5 defuse race | still open |
| **U/E utility + economy** | plan + E0/U0/U1 code — `docs/utility-economy-plan.md` |

G1 live CT split (10 CTs): 3 near A, 2 near B, 5 mid/flex — both sites covered.

### Utility + economy (2026-08-11)

Problem: bots buy nades, walk with them out, never throw; everyone would dump
the same util; no eco discipline.

Practical design (pro 1.6-style):

- **Slots** (`utility.rs`): mid smoke, long smoke, retake flash, … each has
  **one owner** per seed bucket → no 10× mid smoke.
- **Triggers**: execute choke / hold deny / post-plant / contact — **never**
  freezetime or first 3 s (UTIL-1).
- **Economy** (`economy.rs`): Pistol / Eco / Force / Full — eco buys **no**
  rifle, **no** util stack; full buy only purchases **owned** nade kinds.
- **U0 holster**: if holding HE/flash/smoke while walking, `Select` rifle.

Still open: **U2 throw state machine** (pin/release + aim at lineup).

### 2026-08-11 G4+A2e live measure

`docs/metrics-dust2-2026-08-11-g4-a2e.md` — **601** `rung defend` samples (G4 works).
CONGA-1 **0.589** (no win from A2e alone). Binary predated U/E rebuild.

### 2026-08-11 U2 throw machine (shipped + live)

`ThrowMachine` in `utility.rs`: Approach → Select → Aim → Pin → Release → holster.
Wired in `controller` after combat; close threats abort util. Live: `docs/metrics-dust2-2026-08-11-u2.md`.

### 2026-08-11 U2b (lineups + approach timeout)

- `THROW_ARRIVE` 120→220, `APPROACH_TIMEOUT` 5s → throw in place  
- Coords aligned to G1/plant spots; wider execute radii; contact HE aims enemy  
- Live: `docs/metrics-dust2-2026-08-11-u2b.md` — pin+throw+done **8→31**, CONGA-1 **0.619→0.564**

### 2026-08-11 U2c + B1

- Inventory gate via freeze `owned_nades` mask (weapons bitmask not on wire)
- ORCA 160u + forward brake when teammate ahead  
- Live: `docs/metrics-dust2-2026-08-11-u2c-b1.md` — util collapses to real buys only;
  CONGA-1 **0.574** (B1 no win). **231** bot tests.

### 2026-08-11 U2d + A2f

- Util before rifle on force/full; slot buckets 3; A2f stronger opening/lateral/jitter  
- Live: `docs/metrics-dust2-2026-08-11-u2d-a2f.md` — CONGA-1 **0.627** (A2f miss);
  util still ~35; **defend 264 / defuse 212** strong. Offline routes 346 cells.  
- **231** bot tests.

### 2026-08-11 Height-aware pathing + radar layers

**Diagnosis:** Nav gen already multi-level; A→B wall-stare was wrong-floor
`nearest` + Jump/Crouch edges never pressed as buttons.

**Shipped:**
- `route::nearest_prefer_z` / `NavGrid::nearest_prefer_z` — same-floor snap
- PathFollower uses floor-aware replan + hop `required_move` → jump/duck
- Radar: height bands (low/mid/high), jumps orange, crouch purple, ladders green,
  falls blue, narrow doors, bomb goals; layer toggles in Settings

Next: G2 rotate; live verify CT A→B; optional solid-brush boxes on radar.

**Loop discipline:** feature → unit tests → live swarm → metrics doc → next.

---

Stopped 2026-08-03. Everything below is committed; `git log` carries the
reasoning for each change and is worth reading before re-deriving anything.

## State: the bots play

Every gate on the original list is demonstrated **on the server's own log**, on
the full counter-strike-boost stack with the anticheat on:

| | evidence |
|---|---|
| connect, authenticate, spawn | 10/10 bots with real `STEAM_2` ids, zero detections |
| join a team, buy, deploy | `$800 -> $440`, `weapon 17 clip 20` |
| navigate the whole map | `to_goal 2900 -> 19` on de_dust2 |
| shoot, damage, **kill** | 72 kills in a 15-a-side match |
| **plant the bomb** | `triggered "Planted_The_Bomb"`, repeatedly |
| **rescue hostages** | 8 x `Rescued_A_Hostage`, 2 x `All_Hostages_Rescued` on cs_italy |
| survive without drops | 30 bots, 0 client drops, ping 4, loss 0 |

**719 tests passing.**

### The one gate never observed: DEFUSE

Not a defuse bug -- the machine is fine and `Decision` now carries
`bomb_planted`, which proved a client present at the plant decodes it correctly
and one joining later is never told. Two structural facts block observation:

- an **unopposed** plant cannot set one up: with no CTs on the server the round
  ends the moment the bomb is down (~14 s), and CTs need 10-15 s just to connect;
- a **contested** round produces no plants: the carrier dies, or its own
  teammates wipe the CT side and the round ends by elimination in ~20 s, well
  inside the ~50 s a bomb run takes.

To see one: `mp_round_infinite "f"` (blocks `SCENARIO_BLOCK_TEAM_EXTERMINATION`,
`gamerules.h:205`) + `mp_forcerespawn 1` + `mp_c4timer 90`, then a small match.
`scripts/defuse_scenario.sh` gates the CT launch on the planter's OWN decoded
state -- do **not** gate it on the server log, see the traps below.

## 2026-08-10: W7 wired end-to-end (spring view in the loop)

Picked up the uncommitted `aim.rs` spring (`SpringGains`/`ViewMotion`, written
but never wired) and finished the remaining W7 parts. **Builds clean, tests
green** (`cargo test --workspace` with the MSVC toolchain; bot 210, client 184,
nav 135 -- de_dust2-dependent tests need the bsp present). Nothing committed.

What changed, by file:

- `crates/bot/src/aim.rs` -- refactored `step` into `step_with_yaw_error`,
  added `step_guarded` with back-swing guard, two tests (long-way-through-front; guard
  inert when the short way is forward).
- `crates/bot/src/controller.rs` -- `Controller` gained `view_motion:
  ViewMotion`; all five view sites now go through `aim_at`/`aim_at_guarded`:
  combat -> `COMBAT_GAINS`, defuse/hostage look -> `NAV_GAINS`, plant-walk and
  goto -> `NAV_GAINS` + look-ahead + back-swing guard. `Nav` gained
  `look: Option<Vec3>` (`None` = look at steer). `difficulty.max_turn()` is no
  longer used by the controller (kept; tests still cover it).
- `crates/client/src/navigate.rs` -- `PathFollower::look_target(grid, from)`:
  two nodes ahead when both plain (no LADDER/CROUCH/NARROW), |z diff| < 8,
  current radius >= `WIDE_RADIUS`, far point within 384u; else one node ahead;
  `None` when the route is done. Test added.
- `crates/client/src/session.rs` -- fills `Nav.look` from `look_target`.
- `crates/bot/src/idle.rs` -- `AntiIdle::from_seed(seed)`: per-bot amplitude/
  period/phase windows that provably satisfy `guarantees()` (yaw amp 0.9-1.4,
  period 8-16; pitch amp 0.6-1.0, period 7-13). `Controller::new` uses it and
  `debug_assert!(idle.guarantees())`. Test: 64 seeds -> >48 distinct drifts.
- `crates/bot/src/lib.rs` -- exports `SpringGains`, `ViewMotion`, `NAV_GAINS`,
  `COMBAT_GAINS`.
- `scripts/swarm.sh` -- (pre-existing uncommitted) staggered bot lifetimes so
  exits stay under ReAuthCheck's MaxDrop ban window.

Gotchas learned this round (do not rediscover):

- The back-swing guard only fires when current/desired straddle the travel
  bearing with |c - t| >= 180. Test example: current 170, desired -170, travel 0.5.
- `navgrid::flags` has NO `JUMP` constant (jumps are `Move::Jump` links) --
  compile error if you reference `flags::JUMP`.
- `norm_angle` folds into [-180, 180).

Next (in order):

1. **M0 per-tick trace** (prerequisite for verifying W7): **done** --
   `crates/client/examples/view_trace.rs` decodes every `clc_move` from
   `captures/swarm/Bot<NN>.bin.sent` and reports per-tick yaw/pitch/fwd/side/
   buttons with deltas, plus flick episodes (sustained >= 60 deg swings), peak
   turn rate, and overshoot reversals. Run:
   `cargo run -p client --example view_trace -- captures/swarm/Bot01.bin.sent`.
2. **Live run**: 20-30 bot match on de_dust2 with `scripts/swarm.sh` (use the
   staged `life=` pattern -- W3 has still never been measured live either),
   then the section-1 table vs `docs/conga-baseline.md` with
   `python scripts/metrics.py captures/swarm`. W7 targets: peak |dyaw|/s in
   400-900 deg/s on >= 60 deg flicks; overshoot 8-45 deg on >= 50 % of combat
   flicks (was structurally 0); VIEW-1 median >= 6 deg (was 1.1); VIEW-2 <= 15 %
   (was 42.7); 30 distinct anti-idle tuples.
3. **W5 (post-arrival task) -- DONE** (committed): `Controller::post_arrival:
   CampTask`; on arrival with a defend point the bot walks to it and camps
   (hold scaled by fear, view sweep), then re-picks; without one it roams at
   walking speed instead of the old `if arrived { (0, 0) }` stop. `Nav`
   carries `defend_point` and `new_waypoint`; the caller fills them from
   `PathFollower` (defend point picked deterministically per seed+goal,
   300-600u from the goal).
4. **W6 speed spread -- DONE** (committed): `travel()` now takes `remaining`;
   inside `APPROACH_RADIUS` (500u) the bot eases to walking pace; a per-hop
   slowdown dice (`25 * difficulty` percent chance of `0.4 * maxspeed`) rolls
   on each node advance via `Controller::note_node_advance`, consumed in one
   hop. The W1 personality fields (`aggression`, `fear`) now exist and drive
   the camp hold.
5. **W8 last.**

## Current work: humanization (`docs/humanize-plan.md`)

The user's complaint, watching 30 bots: *"predictible moves in lines one behind
another"*. The plan has 18 measured metrics and targets; `docs/conga-baseline.md`
has the numbers.

**Measured live 2026-08-10 (30 bots, 15 min, de_dust2): 14 of 18 targets met.**
Full table in `docs/humanization-after-w1-w7.md`. Highlights:

| metric | baseline | now | target |
|---|---|---|---|
| CT pair-time within 200u | 52.8 % | **10.1 %** | <= 25 % |
| CT median separation | 186u | **1327u** | >= 400u |
| same-team route Jaccard | 0.62 | **0.190** | <= 0.30 |
| STILL-1 (< 1 u/s) | 61.2 % | **7.9 %** | <= 35 % |
| SPEED-1 (fwd == 250) | 79.6 % | **2.6 %** | <= 45 % |
| RUNG-1 (arrived) | 27.2 % | **0.0 %** | <= 10 % |
| VIEW-1 (median |yaw-bearing|) | 1.1 deg | **54.3 deg** | >= 6 deg |

W7 view dynamics verified live via `view_trace`: flicks overshoot (reversed
direction) on every combat flick measured, max 25-29 deg -- structurally
impossible with the old 0.45 ease. Combat happened this time (1,102 combat
samples; the baseline teams never met).

**Still missing (4 of 18):** CONGA-1/2 (68 % / 29 % vs <= 45 % / <= 12 %),
ROUTE-2/3 (0.175 / 915 nodes vs <= 0.15 / >= 1400), PILE-2 max (12 vs <= 5).
All one cause: 15 bots on one bomb site converge into a loose stream on the
final approach. The plan's W2/W4 mechanisms are in but the per-bot goal-node
spread is capped by de_dust2's 2 sites; ROUTE-3 implies more route diversity
is available from the heuristic-weight/edge-jitter levers.


**Not started, in plan order:** W8 (chat/radio), the last optional item, and
the four misses above. W3+W7 are now measured live; the remaining work is the
route-diversity levers (W2/W4 tuning) and a collision-heavy match if combat
verification needs more samples.

## Debug radar GUI (2026-08-10, `debug-gui-radar.md`)

- `crates/gui` (egui/eframe): live radar of all bots. Map silhouette is
  auto-generated from the nav grid (accurate for ANY map, zero per-map art).
  Team-colored dots (T yellow, CT blue, dead grey), **red ring on stuck bots**
  (vel < 1 while requesting fwd for > 3 s), per-bot detail panel, and a
  ~120 s replay scrubber to watch the T-spawn pile-up.
- Telemetry bus: each bot broadcasts a fixed-layout UDP packet every 0.5 s to
  127.0.0.1:27016 when `AIPLAYERS_TELEMETRY_PORT` is set (swarm.ps1 sets it).
  `crates/client/src/telemetry.rs` has the packet + `field_str` helper.
- Run: `scripts/swarm.ps1 -N 30 -Secs 900`, then `cargo run -p gui`.
- Round-start stuck fix (committed `51f21f5`): post-freeze 2 s natural-walker
  grace + teammate-aware unstick (checks same-team players within 60u on the
  push side). First-60s stuck-while-requesting dropped to 2.9%.

## Traps that cost real time. Do not rediscover these.

- **Never poll rcon in a loop, and never mass-kill bots casually.**
  `CheckMaxDrop`: 7 disconnects from one IP in 15 s -> `addip 60.0`, a
  **60-minute ban** that also hits the human sharing the Docker gateway. The
  bridge is now in ReAuthCheck's `[List White IP]`, which exempts only
  `CheckMaxIp`/`CheckMaxDrop`; every authenticity check still applies.
  When rcon *and* the connect handshake both go silent while the container is
  healthy, that is a ban, not a client regression: `docker compose restart`.
- **`rcon log off` ROTATES the log**, so polling it for an event is
  self-defeating -- a plant recorded before the poll lands in a file the next
  poll no longer reads. Cost two runs reporting "no plant yet" while the log
  held one.
- **Semicolon batching does not work**: `rcon "mp_roundtime 3; mp_freezetime 4"`
  sets the cvar to the literal string `"3;"`.
- **`MaxIpNum` accepts 1..31 only.** Out of range is not clamped -- the whole
  parameter is dropped and it falls back to the default of **3**.
- **Locating a message by scanning for its opcode byte is always wrong.** Bitten
  three times: `absorb_baselines`, the stufftext handler, and `cvar_replies`
  (that one queued a reliable per phantom match and jammed the channel at
  `netq 7900`, after which the bot could never send another console command).
  Walk the stream.
- The server log lags in 8 KB blocks and `docker logs` lags with it.
- **`Invalid length` + `badread on opcode clc_fileconsistency` is usually not
  a bad consistency body.** After STEAM auth succeeds, the killer was
  `NetChannel::transmit` putting `reliable_buf` on **every** idle packet while
  a fragment upload was in flight, but without the reliable/fragment flags.
  Server saw a bare `07 <u16 length>` with a short body → drop. Fix: only
  attach reliable/fragment payload when `send_reliable` is true (ReHLDS
  `Netchan_Transmit`). Full write-up: **`docs/remote-join.md`** (section
  “Trap: Invalid length”). Regression:
  `cargo test -p netchan idle_packets_do_not_leak`.

## Reference

- `scripts/swarm.ps1 -N <count> -Secs <seconds>` -- launch bots with unique keys and seeds.
- `scripts/rcon.py "<cmd>"`.
- Tests: `cargo test --workspace`.
