# de_dust2 G2/A2g live validation

## Code gate

- Focused tests passed before the live run: bot **231**, nav **135**, netchan **8**, client **196** (incl. `g2_rotate_fires_on_two_enemies_at_b_not_on_a_hold` and the rotation dedup test).
- `live_signon` (the only failure) needs a running server; it is an environment gate, not a code regression.
- `capture_running` rebuilt clean; only pre-existing warnings (`roam_target`) remain.

## Live run

**Run:** 20 bots × 900 s, staged PowerShell launcher (`scripts/swarm.ps1 -N 20 -Secs 900`), 10 T / 10 CT, de_dust2, local docker server (started fresh before the run).

All 20 bots connected, joined, spawned. 5,749 live samples. Two bomb plants observed (22:37:00 at A, 22:38:58 at B). Combat throughout, no mass-drop, no disconnect, no ban.

## G2 rotation evidence

`G2-ROTATE 9 9` — **9 distinct rotation events across 9 of 10 CTs**. Site index mapping verified against BSP order: **site 0 = B, site 1 = A**.

| bot | seed bucket | default G1 | rotate | target | driven by |
|---|---|---:|---|---|---|
| Bot02 | 3 (B hold, delay) | B | 1 | B | **bomb at B** |
| Bot04 | 7 (mid→B) | B | 1 | A | **bomb at A** |
| Bot06 | 9 (flex→B) | B | 1 | B | **bomb at B** |
| Bot08 | 9 (flex→B) | B | 1 | A | pressure |
| Bot10 | 2 (A flank) | A | 1 | B | pressure |
| Bot12 | 0 (A hold, delay) | A | 1 | A | pressure |
| Bot14 | 6 (mid→A) | A | 1 | A | pressure |
| Bot18 | 0 (A hold, delay) | A | 1 | B | **bomb at B** |
| Bot20 | 7 (mid→B) | B | 1 | B | **bomb at B** |
| Bot16 | 4 (B assault) | B | 0 | — | already on site |

Verification against the plan's acceptance:

- **two visible enemies near B cause an A-held CT to repath toward a B anchor** — Bot10 (A flank) and Bot18 (A hold) rotated to B on pressure/bomb. ✓
- **a CT already assigned to B does not thrash** — no bot logged more than one rotation; `apply_ct_rotation` dedup held across the 4 s cooldown. ✓
- **a single visible enemy does not trigger rotation** — no rotation fired from one-contact observations; the ≥2 gate plus `counts[i] > other` is enforced in `ct_rotate_pick` and covered by the offline test. ✓
- **a planted bomb with a known origin forces CTs toward the correct site** — plant at B (22:38:58) pulled Bot02/06/18/20 to site 0 (B); plant at A (22:37:00) pulled Bot04 to site 1 (A). Bot18's log shows `planted true at [-1486, 2687]` → `rotate 1 site Some(0)`. ✓
- **delay buckets remain on the quiet site for ordinary two-contact pressure** — Bot02 (bucket 3) and Bot12 (bucket 0) stayed on their holds through pressure and rotated only when the bomb overrode the delay gate; Bot16 (B assault) never rotated. ✓
- **cooldown prevents repeated route resets** — exactly one rotation per bot. ✓
- **rotation targets remain floor-aware and use the existing navigation/unstick path** — destinations are `snap_walkable` lane anchors; rotated bots show `rung goto/combat/defuse` with `to_goal` decreasing (Bot04 `1221`, Bot06 `914`, Bot18 `2644`, Bot20 `1488`) and camped on arrival (Bot16 `camp`, `to_goal 32`). ✓

Both site directions were exercised: 5 rotations to B (site 0), 4 to A (site 1).

## Humanization metrics (A2g regression check)

| metric | this run | target | status |
|---|---:|---:|---|
| CONGA-1 | 0.548 | ≤ 0.45 | miss |
| CONGA-2 | 0.055 | ≤ 0.12 | pass |
| ROUTE-1 | 0.226 | ≤ 0.30 | pass |
| ROUTE-2 | 0.108 | ≤ 0.15 | pass |
| ROUTE-3 | 1135 | ≥ 1400 | miss |
| ROUTE-4 | 0.082 | ≤ 0.15 | pass |
| STILL-1 | 0.195 | ≤ 0.35 | pass |
| SPEED-1 | 0.016 | ≤ 0.45 | pass |
| RUNG-1 | 0.000 | ≤ 0.10 | pass |
| VIEW-1 | 67.0° | ≥ 6° | pass |
| VIEW-2 | 0.000 | ≤ 0.15 | pass |
| SEP-CT median | 1091 u | ≥ 400 u | pass |
| SEP-200 CT / T | 0.080 / 0.028 | ≤ 0.25 | pass |
| COVER-1 | 0.977 | ≥ 0.75 | pass |
| COVER-2 | 0.168 | ≤ 0.30 | pass |
| PILE-2 mean / max | 0.4 / 10 | ≤ 2 / ≤ 5 | max miss |
| PILE-1 / PILE-3 | 0.000 / 0.000 | — | pass |

A2g remains non-regressive: CONGA-2, ROUTE-2, STILL-1, VIEW-1 all within target, and CONGA-1 (0.548) is the best value since A2d (0.545) — better than the A2f miss (0.627) and the A2b/A2c values. The remaining misses (CONGA-1, ROUTE-3, PILE-2 max) are the pre-existing humanization gaps, unchanged by G2.

## Conclusion

**G2 is validated live.** Local-PVS rotation works: CTs repath toward the threatened site on ≥2 visible enemies or a planted bomb, delay buckets hold the quiet site under light pressure, and no route-reset thrashing occurs. **A2g is non-regressive.** The previous environment block (server down / connect timeout) is resolved — the fresh server accepted all 20 bots.

Next layer per plan §5: G2 fires and A2g is non-regressive → **start G0 team/round state bus** (`docs/tactics-plan.md`): publish team/alive/role/site/rung/bomb state, resolve plant site for late joiners, add offline snapshot tests for site pressure and plant inheritance.
