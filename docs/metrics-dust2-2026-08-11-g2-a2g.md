# de_dust2 G2/A2g validation

## Code gate

- Full workspace tests passed before the live run.
- Focused tests passed: bot **231**, client **195** before instrumentation.
- After adding the G2 diagnostic counter, client library tests passed: **196**.
- `capture_running` rebuilt successfully.
- Existing warnings remain (`roam_target`, one unused import, one unnecessary `mut`); no new warning is from the diagnostic.

## First staged live snapshot

**Run:** 20 bots × 900 s, staged PowerShell launcher, pre-instrumentation G2/A2g binary.

The run connected and produced 5,638 live samples. G2 event counts cannot be recovered from this snapshot because the old binary only logged the static `role split` label; the movement metrics are still valid:

| metric | value | target | status |
|---|---:|---:|---|
| CONGA-1 | 0.573 | ≤ 0.45 | miss |
| CONGA-2 | 0.051 | ≤ 0.12 | pass |
| SEP-CT median | 980 u | ≥ 400 u | pass |
| SEP-200 CT / T | 0.071 / 0.036 | ≤ 0.25 | pass |
| COVER-1 | 0.976 | ≥ 0.75 | pass |
| COVER-2 | 0.171 | ≤ 0.30 | pass |
| ROUTE-1 | 0.240 | ≤ 0.30 | pass |
| ROUTE-2 | 0.104 | ≤ 0.15 | pass |
| ROUTE-3 | 1121 | ≥ 1400 | miss |
| ROUTE-4 | 0.077 | ≤ 0.15 | pass |
| STILL-1 | 0.208 | ≤ 0.35 | pass |
| SPEED-1 | 0.022 | ≤ 0.45 | pass |
| VIEW-1 | 66.7° | ≥ 6° | pass |
| VIEW-2 | 0.000 | ≤ 0.15 | pass |
| PILE-2 mean / max | 0.4 / 10 | ≤ 2 / ≤ 5 | max miss |

A2g is non-regressive on the previously passing route/pair metrics. CONGA-1 and ROUTE-3 remain the primary humanization misses; do not increase A2 scales based on this run because the earlier A2f crank regressed CONGA-1.

## Diagnostic change

The instrumented binary now logs:

```text
obj: ... | rotate <cumulative-count> site Some(<site-index>)
```

`Session::apply_ct_rotation` increments the counter only for a new `(site, destination)` target and ignores repeated PVS observations. `refresh_objective` resets it per round. The parser reports `G2-ROTATE <events> <bots>` and handles counter resets between rounds. The new deduplication test passes.

## Instrumented rerun

**Run:** 20 bots × 300 s, staged PowerShell launcher.

Blocked before signon: all 20 logs contain `signon failed: connect timeout`, and `docker compose ps` showed no running server service. The run produced no live samples and no G2 evidence. This is an environment/server availability failure, not a behavior result.

## Next action

Start the test server and confirm it is healthy before launching another staged run:

```powershell
docker compose up -d
cargo build -p client --example capture_running
powershell -File scripts/swarm.ps1 -N 20 -Secs 900
python scripts/metrics.py captures/swarm
```

Use the new `G2-ROTATE` line and the `rotate` suffix in bot logs to validate actual rotations. Keep G0 pending until that run proves G2 behavior or demonstrates that local PVS is insufficient.
