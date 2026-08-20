# de_dust2 U2d util owners + A2f route bias

**When:** 2026-08-11, 20 bots × 900s  
**Changes:**
- Force/full buy owned util *before* rifle  
- Slot `team_buckets` 5→3 (~1/3 owners)  
- A2f: OPENING 125, LATERAL 130 / 6 lanes, edge jitter 0..90  

## Humanize

| metric | U2c+B1 | **U2d+A2f** | target |
|---|---|---|---|
| live | 5707 | **5592** | — |
| CONGA-1 | 0.574 | **0.627** | ≤0.45 **worse** |
| CONGA-2 | 0.056 | **0.072** | ≤0.12 pass |
| ROUTE-2 | 0.097 | **0.108** | ≤0.15 pass |
| ROUTE-3 | 1043 | **997** | ≥1400 miss |
| PILE-2 max | 9 | **10** | ≤5 |
| STILL-1 | 0.218 | **0.222** | ≤0.35 pass |

**A2f crank failed** — more bias did not help CONGA (possibly worse corridor packing). Revert candidate next loop if still bad.

## Util

| | U2c | **U2d** |
|---|---|---|
| util total | 32 | **35** |
| pin+throw+done | 3 | **4** |

Still sparse — ownership/money still limit live throws. Offline 30 seeds → **346** route cells (up from 261).

## Wins

| rung | samples |
|---|---|
| **defend** | **264** (up from 112) |
| **defuse** | **212** (up from 69) |

G4/G5 pathing is healthy even when util is quiet.

## Next

1. **Revert A2f scales** or keep offline diversity only if live CONGA regresses again.  
2. **G2 CT rotate** on site wipe (bigger spectator win than util volume).  
3. Util: grant flash to *all* full-buy bots once/round (not only slot hash).  
