# Baseline: how much the bots clump, before any humanization

Measured from `captures/swarm/bot*.log` of a live 15-a-side match on de_dust2,
30 bots, telemetry sampled every 2 s. Teams are odd/even bot index, matching
`scripts/swarm.sh`.

For every pair of same-team bots at every shared sample time, the horizontal
distance between them:

| | Terrorists (14) | Counter-terrorists (15) |
|---|---|---|
| pair-samples | 40 092 | 46 776 |
| **within 200 units of a teammate** | **33.5 %** | **52.8 %** |
| median separation | 441 units | **186 units** |
| 10th percentile | 11 units | 8 units |

A 10th percentile of 8-11 units means the two bots are inside each other's
bounding boxes: 32 units wide, so they are standing on the same spot.

The counter-terrorists are the worse half, and that points at the cause rather
than the symptom. Every bot on a team is given the *same* objective point and
walks to it with the *same* A\* over the *same* lattice, so the routes are not
merely similar, they are identical; and once there, `rung arrived` parks them
all on one coordinate. The T side is only better because its route is longer,
so combat scatters it on the way.

## How to re-measure

The one-liner that produced this is in the session log; the shape is:

- parse `t+ <n>s origin [x y z]` out of each `captures/swarm/bot*.log`
- for each same-team pair, at each shared `t`, take the 2-D distance
- report the fraction under 200 units, the median, and the 10th percentile

Any change aimed at the conga line has to move these numbers, and the target is
the **CT** column -- a median separation comparable to the T side's 441 units,
and the sub-200 fraction well under half.
