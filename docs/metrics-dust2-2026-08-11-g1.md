# de_dust2 after Phase G1 (CT default anchors)

**When:** 2026-08-11 ~ live 20 bots × 900s  
**Code:** `dust2_ct_setup` — 10-bucket A/B/mid/flex named anchors for CT

## Humanize table

| metric | A2d | **G1** | target |
|---|---|---|---|
| live | 6448 | **5778** | — |
| CONGA-1 | 0.545 | **0.574** | ≤0.45 |
| CONGA-2 | 0.054 | **0.055** | ≤0.12 pass |
| ROUTE-2 | 0.150 | **0.091** | ≤0.15 **pass** |
| ROUTE-3 | 1222 | **1017** | ≥1400 |
| PILE-2 max | 10 | **10** | ≤5 |
| STILL-1 | 0.210 | **0.265** | ≤0.35 pass |

## CT setup (G1 acceptance)

From even bots' first `objective:` line (10 CTs):

| near A (&lt;900u) | near B | mid/flex |
|---|---|---|
| **3** | **2** | **5** |

Both sites held + mid/connector presence. Unit test
`dust2_ct_setup_covers_both_sites_and_mid` passes offline.

## Next

G4 T post-plant holds + A2e earlier lateral (MIN 350→200) built for next swarm.
G2 rotation still needs team bus (G0).
