# Phase 0 — de_dust2 baseline freeze checklist

Run this **before** claiming any later phase improved the bots. No behaviour
change is required for Phase 0 itself; it freezes the measurement harness.

## Canonical live run

```powershell
# From aiplayers-rs/, with the test server stack healthy and not IP-banned.
.\scripts\swarm.ps1 -N 30 -Secs 900
python scripts\metrics.py captures\swarm
```

Save the metrics table to:

`docs/metrics-dust2-YYYYMMDD.md`

## Required observations

| Check | How | Pass |
|---|---|---|
| 30 bots connected | server log / radar | yes |
| Teams meet (combat) | `combat` rung samples in metrics | > 500 preferred |
| Plant happens | server log `Planted_The_Bomb` | ≥ 1 |
| No mass drops | server, 0 bans | yes |
| Humanize table | `metrics.py` | compare to `humanization-after-w1-w7.md` |
| View dynamics | `cargo run -p client --example view_trace -- captures/swarm/Bot01.bin.sent` | overshoot present on combat flicks |
| GUI smoke | `cargo run -p gui` while swarm runs | map silhouette + dots |

## Known post-W7 misses (targets to beat)

| Metric | Latest (2026-08-10) | Target |
|---|---|---|
| CONGA-1 | ~67–68 % | ≤ 45 % |
| CONGA-2 | ~9.7–29 % | ≤ 12 % |
| ROUTE-2 | ~0.17–0.19 | ≤ 0.15 |
| ROUTE-3 | ~900–915 / 4715 | ≥ 1400 |
| PILE-2 max | 12–15 | ≤ 5 |

## Traps

- Do not mass-kill bots (ReAuthCheck MaxDrop → 60 min ban).
- Do not poll with `rcon log off` (rotates the log).
- Do not batch cvars with `;` in one rcon string.

## After Phase A1 (roles + approach rings)

Re-run the same protocol. Expect movement first on CONGA-1, PILE-2 max, and
destination diversity in bot logs (`objective: ... role hold|assault|flank|split`).
