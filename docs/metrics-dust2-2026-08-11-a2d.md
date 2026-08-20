# de_dust2 full A2d (4 lateral lanes)

**When:** 2026-08-11 ~19:52–20:09 local  
**Fleet:** 20 bots × 900s staged, de_dust2  
**Binary:** multi-lane lateral bias (`LATERAL_LANES=4`, scale 95)

| metric | A2c full | **A2d full** | target | |
|---|---|---|---|---|
| live samples | 6170 | **6448** | ≥1500 | ok |
| CONGA-1 | 0.567 | **0.545** | ≤0.45 | miss (slight) |
| CONGA-2 | 0.041 | **0.054** | ≤0.12 | pass |
| ROUTE-2 | 0.130 | **0.150** | ≤0.15 | at edge |
| ROUTE-3 | 1082 | **1222** | ≥1400 | miss (↑140) |
| PILE-2 mean/max | 0.4 / 10 | **0.5 / 10** | ≤2 / ≤5 | max miss |
| STILL-1 | 0.271 | **0.210** | ≤0.35 | pass |
| COVER-1 | 0.987 | **0.982** | ≥0.75 | pass |

## Decision

Lanes help **ROUTE-3** more than CONGA-1. Still need a different lever for
conga (stronger local avoid / mid-round goal redraw / earlier lateral band)
before cranking scale further.

Also noted live: synchronized jump at freezetime end (stuck timers during
freeze) — fixed in code after this run (`session` skip `next_waypoint` while
frozen + `hold()` clears origin-stuck).
