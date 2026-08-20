# de_dust2 G4 post-plant + A2e lateral (pre U/E buy rebuild)

**When:** 2026-08-11, 20 bots × 900s  
**Binary:** `capture_running` 20:33 (G1 CT anchors + G4 DefendPlant + A2e LATERAL_MIN 200)  
**Note:** economy/utility holster landed *after* this binary — next swarm needs rebuild.

## Humanize

| metric | G1 | **G4+A2e** | target |
|---|---|---|---|
| live | 5778 | **5679** | — |
| CONGA-1 | 0.574 | **0.589** | ≤0.45 |
| CONGA-2 | 0.055 | **0.087** | ≤0.12 pass |
| ROUTE-2 | 0.091 | **0.099** | ≤0.15 pass |
| ROUTE-3 | 1017 | **1009** | ≥1400 |
| PILE-2 max | 10 | **10** | ≤5 |
| STILL-1 | 0.265 | **0.212** | ≤0.35 pass |

A2e alone did not move CONGA-1 (noise / still corridor-bound).

## G4 signal

| signal | value |
|---|---|
| `rung defend` log samples | **601** |
| plant-related log mentions | 194 |

T post-plant hold path is firing live (was Idle before G4).

## Next

1. Rebuild with E0/U0/U1 (eco buy + nade holster + slots).  
2. U2 throw state machine.  
3. CONGA still needs a stronger lever (ORCA / mid-round redraw), not more lateral min.
