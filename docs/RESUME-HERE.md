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

## Current work: humanization (`docs/humanize-plan.md`)

The user's complaint, watching 30 bots: *"predictible moves in lines one behind
another"*. The plan has 18 measured metrics and targets; `docs/conga-baseline.md`
has the numbers.

**Done and measured (W1, W4, W3):**

| metric | baseline | now | target |
|---|---|---|---|
| CT pair-time within 200u | 52.8 % | **12.9 %** | <= 25 % |
| CT median separation | 186u | **1603u** | >= 400u |
| same-team route Jaccard | 0.62 | **0.17** | <= 0.30 |

Cross-team Jaccard was 0.07 -- the "unrelated players" floor. 0.17 is near it.

W3 (node radius, destination jitter, radius arrival, A\* post-smoothing) is
committed and green but has **not had a live run**: the numbers above were
measured *without* it. First job tomorrow is a 20-30 bot match to see what it
does to them.

**Not started, in plan order:** speed variation (79.6 % of walking samples are
exactly `fwd 250.0`), view behaviour when not fighting (median |yaw - bearing|
is 1.1 deg -- they stare straight at their destination; 42.7 % of consecutive
samples share an identical integer yaw), and the combat/view metrics, which need
a match where the teams actually **collide** -- in the baseline run they barely
met, so those 611 combat samples cannot support any claim.

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
