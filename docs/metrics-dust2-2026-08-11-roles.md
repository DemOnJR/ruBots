# de_dust2 metrics after Phase A1 roles + approach rings

**Run:** 2026-08-11, Docker `aiplayers-cs16`, 30 bots (15v15), ~15 min  
`scripts/swarm.ps1 -N 30 -Secs 900` then `python scripts/metrics.py captures/swarm`  
(parser updated to accept optional `role <name>` on `obj:` lines)

## Role assignment (from bot logs)

| role | count |
|---|---|
| assault | 14 |
| hold | 7 |
| flank | 6 |
| split | 3 |

Hold/Flank destinations sit on a 220–450u ring; plant spots remain inside bomb volumes. Carriers override to plant_spot.

## Server log outcomes

| event | count |
|---|---|
| Planted_The_Bomb | **9** |
| Target_Bombed | 8 |
| Defused_The_Bomb | 0 |
| kills | 229 |
| combat rung samples | 555 |

## Humanize table vs prior natural-walker baseline

| id | metric | W1–W7 + walker | **this run (roles)** | target | status |
|---|---|---|---|---|---|
| PILE-1 | `arrived` busiest 64u cell | 0.0 % | **0.0 %** | ≤ 20 % | pass |
| PILE-2 | mean / max in 192u ball | 1.4 / 15 | **2.06 / 6** | ≤ 2.0 / ≤ 5 | **max near miss** |
| PILE-3 | share in that box | 0.0 % | **0.0 %** | ≤ 25 % | pass |
| CONGA-1 | trail within 100u ≤10 s | 67.3 % | **71.3 %** | ≤ 45 % | miss |
| CONGA-2 | pair-time ≤300u moving | 9.7 % | **7.3 %** | ≤ 12 % | **pass** |
| SEP-CT | CT median separation | 1270 u | **801 u** | ≥ 400 u | pass |
| SEP-200 | pair ≤200u CT / T | 10.8 / — | **8.6 / 8.4 %** | ≤ 25 % | pass |
| COVER-1 | cells per bot | 0.962 | **0.970** | ≥ 0.75 | pass |
| COVER-2 | top-5 cell share | 15.2 % | **14.0 %** | ≤ 30 % | pass |
| ROUTE-1 | (script) cell Jaccard | 0.209 | **0.192** | ≤ 0.30 | pass |
| ROUTE-2 | waypoint Jaccard | 0.191 | **0.109** | ≤ 0.15 | **pass** |
| ROUTE-3 | distinct nodes / 4715 | 904 | **899** | ≥ 1400 | miss |
| ROUTE-4 | top-20 node share | — | **9.7 %** | ≤ 15 % | pass |
| STILL-1 | vel < 1 | 9.6 % | **27.8 %** | ≤ 35 % | pass (regressed) |
| RUNG-1 | `arrived` | 0.0 % | **0.0 %** | ≤ 10 % | pass |
| SPEED-1 | fwd == 250 | 2.6 % | **~2 %** | ≤ 45 % | pass |
| VIEW-1 | median \|yaw−bearing\| | 52.7° | **61.9°** | ≥ 6° | pass |
| VIEW-2 | identical int yaw | 0.0 % | **0.0 %** | ≤ 15 % | pass |

Live samples: **8445** (of 13761 total including dead/freeze).

## Readout

**Wins**
- **PILE-2 max 15 → 6** (target ≤5): approach rings + roles almost kill the plant-disc dogpile.
- **ROUTE-2 0.191 → 0.109**: waypoint sets diversify enough to pass.
- **CONGA-2 passes** cleanly; plants still happen (9).
- All four roles appear in the wild; carrier plant override works.

**Still open (same root causes as plan A2/B)**
- **CONGA-1 ~71%**: bots still stream on shared corridors even when endpoints differ.
- **ROUTE-3 ~900**: role destinations alone do not force different A* corridors — need stronger edge jitter / opening bias (Phase A2).
- **STILL-1 up to 28%**: Hold/camp/defuse time; still under target but watch when adding ORCA.

## Next code steps (unchanged plan)

1. Phase A2 — raise route diversity (offline ≥12 sequences T-spawn→A; live ROUTE-3).
2. Phase B — ORCA-lite on visible teammates for CONGA-1.
3. Phase D — plant-spot library + defuse scenario (0 defuses this run).
4. Phase E — GUI role colors + live fleet metrics.
