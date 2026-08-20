# de_dust2 U2c inventory gate + B1 ORCA brake

**When:** 2026-08-11, 20 bots × 900s  
**Changes:**
- Util only if freeze buy granted that nade kind (`owned_nades` mask)
- ORCA radius 120→160, stronger push; forward soft-brake when teammate ahead

## Humanize

| metric | U2b | **U2c+B1** | target |
|---|---|---|---|
| live | 5723 | **5707** | — |
| CONGA-1 | 0.564 | **0.574** | ≤0.45 |
| CONGA-2 | 0.045 | **0.056** | ≤0.12 pass |
| ROUTE-2 | 0.091 | **0.097** | ≤0.15 pass |
| ROUTE-3 | 1089 | **1043** | ≥1400 |
| PILE-2 max | 10 | **9** | ≤5 |
| STILL-1 | 0.256 | **0.218** | ≤0.35 pass |

B1 did **not** move CONGA-1 (noise / corridor topology). STILL slightly better.

## Util (inventory gate expected drop)

| | U2b | **U2c** |
|---|---|---|
| util total | 286 | **32** |
| util-walk | 235 | **28** |
| pin+throw+done | 31 | **3** |

Fewer fake throws without a bought nade — correct. Next: more full-buy util owners on full rounds, or always-buy one flash for slot owners on force+.

## Other

| rung | samples |
|---|---|
| defend | 112 |
| defuse | 69 |
