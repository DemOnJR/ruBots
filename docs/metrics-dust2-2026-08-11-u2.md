# de_dust2 U2 util throw + E0 eco + G4/G1 stack

**When:** 2026-08-11, 20 bots × 900s  
**Binary:** `capture_running` ~20:57 (ThrowMachine + economy + holster + G1/G4)

## Humanize

| metric | G4+A2e | **U2** | target |
|---|---|---|---|
| live | 5679 | **5392** | — |
| CONGA-1 | 0.589 | **0.619** | ≤0.45 miss |
| CONGA-2 | 0.087 | **0.053** | ≤0.12 pass |
| ROUTE-2 | 0.099 | **0.076** | ≤0.15 **pass** |
| ROUTE-3 | 1009 | **1041** | ≥1400 miss |
| PILE-2 max | 10 | **9** | ≤5 miss |
| STILL-1 | 0.212 | **0.260** | ≤0.35 pass |

CONGA slightly worse (util-walk congestion?). ROUTE-2 improved.

## Util rungs (U2)

| rung | samples |
|---|---|
| util-walk | **189** |
| util-select | 11 |
| util-pin | 1 |
| util-throw | **6** |
| util-done | **1** |
| **util total** | **208** |

Throws are happening (not freezetime-only spam). Completions low vs walks —
lineup `from` points need nav-snap / closer triggers so more bots finish pin.

## Other signals

| rung | samples |
|---|---|
| defend (G4) | 154 |
| **defuse** | **196** (first real volume!) |
| plant / plant-walk | 15 / 177 |

## Next

1. Snap utility `from`/`aim` to nav grid (like plant spots).  
2. Broaden ExecuteChoke radii or attach slots to G1 anchors.  
3. CONGA lever independent of util.  
