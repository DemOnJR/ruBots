# de_dust2 metrics after A1 roles + A2 route diversity + B ORCA-lite + C lead + D plants

**Run:** 2026-08-11, 30 bots × 15 min, code stack A1–E (before final ORCA/jitter bump).

## Offline gate (A2)

`thirty_seeds_produce_many_distinct_dust2_routes`: **30/30 distinct** T-spawn→A routes, 209 distinct 80u cells.

## Live table

| id | roles-only | **A2+B+C+D** | target | status |
|---|---|---|---|---|
| PILE-2 max | 6 | **6** | ≤5 | near |
| CONGA-1 | 71.3% | **65.8%** | ≤45% | miss (improving) |
| CONGA-2 | 7.3% | **5.6%** | ≤12% | **pass** |
| ROUTE-2 | 0.109 | **0.155** | ≤0.15 | near miss |
| ROUTE-3 | 899 | **1261** | ≥1400 | miss (big jump) |
| ROUTE-4 | 9.7% | **8.8%** | ≤15% | pass |
| STILL-1 | 27.8% | **23.2%** | ≤35% | pass (better) |
| SEP-200 CT/T | 8.6/8.4% | **7.8/4.8%** | ≤25% | pass |
| COVER-1 | 0.97 | **0.971** | ≥0.75 | pass |
| VIEW-1 | 61.9° | **56.4°** | ≥6° | pass |
| Plants | 9 | **12** | — | good |

Live samples: 9368.

## Notes

- ROUTE-3 +362 nodes from roles-only run — opening bias + stronger jitter works.
- CONGA-1 still dominated by shared mid-map corridors; ORCA radius bumped further after this run.
- VIEW-2 script figure can be misleading (pairs across bots at same team/time, not per-bot continuity).
- After disconnects, `docker compose restart` to clear ReAuth bans.
