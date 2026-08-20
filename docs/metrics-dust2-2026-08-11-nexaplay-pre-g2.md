# de_dust2 Remote Live 20-bot (pre G2 / A2g)

**When:** 2026-08-11, remote dedicated server, N=20, ~10 min  
**Binary:** post-fileconsistency netchan fix; **before** A2f revert + G2 rotate  
**Logs:** `captures/swarm/bot*.log` (live sample 1702)

## Humanize

| metric | value | target |
|---|---|---|
| live | 1702 | — |
| CONGA-1 | **0.579** | ≤0.45 miss |
| CONGA-2 | **0.097** | ≤0.12 pass |
| ROUTE-2 | **0.052** | ≤0.15 pass |
| ROUTE-3 | **653** | ≥1400 miss (short remote run) |
| PILE-2 | **0.91** mean | — |
| STILL-1 | **0.127** | ≤0.35 pass |

Remote join works end-to-end (auth + spawn + team + movement). CONGA-1 still the main humanize miss; A2f scales were reverted in the next loop (G2/A2g).

## Next (implemented after this snapshot)

1. Revert A2f opening/lateral/jitter crank (CONGA-1 regressor on local U2d).  
2. **G2** CT rotate on ≥2 PVS enemies at a site (delay Hold stays home).  
3. Shared full-buy **flash** for all force/full buyers.
