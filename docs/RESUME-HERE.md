# Where we are, and what to do next

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
  added `step_guarded` porting YaPB's back-swing guard verbatim
  (`yapb/src/vision.cpp:172-195`), two tests (long-way-through-front; guard
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
  bearing with |c - t| >= 180 -- with travel exactly 0 YaPB's `fzero(forward)`
  skips it (ported as `travel_yaw.abs() > 1e-4`). Test example: current 170,
  desired -170, travel 0.5.
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

## Reference

- YaPB cloned read-only at `D:\Downloads\app\yapb` (`src/navigate.cpp`,
  `planner.cpp`, `combat.cpp`, `botlib.cpp`). ReHLDS/ReGameDLL at
  `D:\Downloads\app\{rehlds,regamedll}`. **Never edit any of them.**
- `scripts/swarm.sh N SECS` -- N bots, alternating teams, own key and seed each.
- `scripts/rcon.py "<cmd>"`, `crates/client/examples/mapinfo.rs`.
- Tests need MSVC: `rustup run stable-x86_64-pc-windows-msvc cargo test --workspace`.
  `os error 5` is antivirus on a freshly linked test exe -- re-run.
