# de_dust2 full A2c (LATERAL_BIAS_SCALE 95, binary L/R)

**When:** 2026-08-11 ~18:55–19:17 local  
**Fleet:** 20 bots × 900s staged, de_dust2  
**Binary:** `capture_running` built 18:54 (A2c)

| metric | A2c early (~4 min) | **A2c full** | A2b (N=30) | target | |
|---|---|---|---|---|---|
| live samples | 1637 | **6170** | 8807 | ≥1500 | ok |
| CONGA-1 | 0.596 | **0.567** | 0.684 | ≤0.45 | miss (improving) |
| CONGA-2 | 0.073 | **0.041** | 0.052 | ≤0.12 | **pass** |
| ROUTE-2 | 0.071 | **0.130** | 0.130 | ≤0.15 | **pass** |
| ROUTE-3 | 622 | **1082** | 1276 | ≥1400 | miss |
| PILE-2 mean/max | 1.1 / 10 | **0.4 / 10** | — / 5 | ≤2 / ≤5 | max miss |
| STILL-1 | 0.254 | **0.271** | 0.286 | ≤0.35 | pass |
| COVER-1 | 0.970 | **0.987** | — | ≥0.75 | pass |
| SEP-200 CT/T | — | **0.080 / 0.035** | — | ≤0.25 | pass |
| Plants | — | ≥1 (Bot15) | 9 | — | ok |

## Decision

Scale-only A2c helped CONGA-1 modestly (~60% → **56.7%**) but still far from 45%.
Binary left/right still packs half the team on one wall of each corridor.

**Next (A2d):** four lateral *lanes* (target cross −0.75/−0.25/+0.25/+0.75)
instead of binary L/R; keep `LATERAL_BIAS_SCALE = 95`.
