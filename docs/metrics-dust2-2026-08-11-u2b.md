# de_dust2 U2b — util approach timeout + better lineups

**When:** 2026-08-11, 20 bots × 900s  
**Change:** wider THROW_ARRIVE (220), APPROACH_TIMEOUT 5s throw-in-place, G1/plant-aligned coords, wider triggers, contact HE aim

## Humanize

| metric | U2 | **U2b** | target |
|---|---|---|---|
| live | 5392 | **5723** | — |
| CONGA-1 | 0.619 | **0.564** | ≤0.45 |
| CONGA-2 | 0.053 | **0.045** | ≤0.12 pass |
| ROUTE-2 | 0.076 | **0.091** | ≤0.15 pass |
| ROUTE-3 | 1041 | **1089** | ≥1400 |
| PILE-2 max | 9 | **10** | ≤5 |
| STILL-1 | 0.260 | **0.256** | ≤0.35 pass |

## Util

| rung | U2 | **U2b** |
|---|---|---|
| util-walk | 189 | **235** |
| util-pin | 1 | **13** |
| util-throw | 6 | **16** |
| util-done | 1 | **2** |
| util total | 208 | **286** |
| pin+throw+done | 8 | **31** |

**Decision:** U2b clearly improves throw completions. Still few `util-done` (2s telemetry undersamples short SM). Next: inventory gate (only throw if nade bought), more post-plant owners, CONGA ORCA.
