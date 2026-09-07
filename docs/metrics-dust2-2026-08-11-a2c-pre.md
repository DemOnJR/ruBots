# de_dust2 live snapshot (A2b binary, early swarm)

**When:** 2026-08-11 ~18:52 local  
**Fleet:** 20 bots, de_dust2, ~4 min into 900s run (server healthy, not banned)  
**GUI:** restarted (pid later)

| metric | value | target | |
|---|---|---|---|
| live samples | **1637** | ≥1500 to decide | ok |
| CONGA-1 | **0.596** | ≤0.45 | miss |
| CONGA-2 | 0.073 | ≤0.12 | pass |
| ROUTE-2 | 0.071 | ≤0.15 | pass |
| ROUTE-3 | **622** | ≥1400 | miss (early; full A2b was 1276) |
| PILE-2 mean/max | 1.1 / **10** | ≤2.0 / ≤5 | max miss (spawn pile?) |
| STILL-1 | 0.254 | ≤0.35 | pass |
| COVER-1 | 0.970 | ≥0.75 | pass |

**Action this fire:** A2c — LATERAL_BIAS_SCALE 60→95 only; rebuild + staged restart N=20.
