# Multi-level nav + radar layers

## What was wrong

Nav **generation is already height-aware** (stacked floors per XY column, jump/
fall/crouch/ladder edges, 3D A*). The CT “stuck on A looking at a wall, can’t
get to B” symptom was mostly:

1. **Wrong-floor snap** — pure 3D `nearest` could pick a tunnel node under A/mid
   when the bot stood on the platform (XY closer through a column).
2. **Jump/crouch never pressed** — graph edges knew `Move::Jump` / `Crouch`, but
   the client only jumped via unstick, so lips/boxes were walked as flat ground.

Radar only drew XY walkable dots (one color) — underpasses, jumps, doors
invisible.

## Fixes shipped

| Area | Change |
|------|--------|
| `nav::route::nearest_prefer_z` | Penalize `|Δz| > 40` so same floor wins |
| `PathFollower::replan` | Uses floor-aware nearest for start/goal |
| `PathFollower::required_move` | Exposes hop kind for current edge |
| `session` | Sets `jump` / `duck` from hop kind |
| Radar | Height bands + hop overlays + toggles |

## Radar legend

| Layer | Color |
|-------|--------|
| Low / under (tunnels) | blue-grey |
| Mid | neutral grey |
| High (A platform) | olive |
| Jump edges | orange |
| Crouch / narrow door | purple |
| Ladder | green |
| Fall / drop | light blue |
| Bomb site nodes | red |

Toggles: Settings tab (height + special paths).

## Run radar

```powershell
# bots running with AIPLAYERS_TELEMETRY_PORT=27016
cargo run -p gui
# or
powershell -File scripts/start-gui.ps1
```

## Not yet

- Solid brush / box outlines from BSP entities on radar  
- Explicit door entities (doors are walkable openings in the graph)  
- G2 CT rotate when site wiped  
