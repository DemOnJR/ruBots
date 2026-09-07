# The crosshair: why it shook, and what it does now

**Symptom reported:** bots holding a position have a crosshair that shakes.
They do not look around like a player, and they do not turn toward sounds.

## What it actually was, measured

Two instruments, because "the crosshair looks wrong" is not a measurement:

* `cargo run -p client --example look_trace -- <capture>.bin.sent` decodes our
  own outgoing commands with the server's own `usercmd_t` table and reports how
  the sent view moved.
* `Session::ViewStats` does the same live, and the state block prints it next
  to the rung — which is what made the answer obvious, because the numbers are
  only damning on one rung:

| rung | reversals/s | dwell |
| --- | --- | --- |
| goto | 1.98 | 0.66 s |
| defuse | 2.19 | 1.26 s |
| plant-walk | 1.80 | 0.63 s |
| defend | 2.73 | 0.25 s |
| dead | 0.37 | 4.03 s |
| **camp** | **18.52** | **0.13 s** |

Eighteen and a half changes of yaw direction per second while holding, against
two on every rung that walks. That is the shake, and it is not subtle once
there is a number for it.

### The cause

Not the sweep, and not the anti-idle drift — both of those were suspects and
both were measured innocent (removing them changed the rate by nothing). It is
one line in the navigation rung:

```rust
let look_eye = nav_look_point(world.me.origin, look);
let look_angles = aim_angles(world.me.origin, look_eye);
self.aim_at_guarded(look_angles, NAV_GAINS, dt, ...);
```

`aim_angles` is a **bearing**, and on arrival `look` is the objective itself,
which by then is under the bot's feet. The bearing to a point half a unit away
is whatever that frame's sub-unit origin jitter says it is — the origin a
server reports for a body standing still is never the same twice — so the
desired angle was a different answer every tick and the aim spring chased every
one of them. It ran *before* the camp behaviour each tick, so whatever the hold
decided was immediately overwritten.

Reproduced offline at 12.65 reversals/s with a median swing of **2.19° per
tick** (p95 7.4°, max 18.2°), from nothing but ±0.5 units of origin jitter and a
variable frame time. With a static origin and a fixed `dt` the bug is invisible,
which is why it survived the existing tests.

**The fix**: do not aim at a point you are standing on. Below `MIN_LOOK_RANGE`
(96 units) the head simply does not move, and the behaviour that owns the hold
supplies the angle instead. Offline that takes the same case from 12.65
reversals/s to **0.00**, with the only remaining motion the anti-idle ramp
creeping 0.0008° per tick.

`controller::tests::a_holding_bot_does_not_shake_its_crosshair` pins it, in both
the with-sight-lines and fallback cases, and it fails at 11.9 reversals/s
against the old code — verified by reverting the one line.

## What it does now

### Sight lines, from the map

`nav::watch::watch_points` answers "where can someone come from that I can see
from here", using the two things the project already has: the nav lattice knows
where a player can walk, and the BSP hulls answer what is visible. Nodes within
900 units are bucketed by bearing, traced for line of sight with `Hull::Point`,
and the farthest visible one per direction is kept — then filtered so no two
kept directions are within 35° of each other, because a defender watching two
angles ten degrees apart has wasted one of them.

The client computes them once per defend point (a lattice sweep plus a trace
per direction is far too much per tick) and hands them to the brain on `Nav`.

### A look model, not a sweep

With the tremor gone, the hold needed something to actually *do*. The old
behaviour lerped the look target from the defend point to a point 30 % back
toward the goal across the whole hold, so the crosshair crept without ever
stopping, over an arbitrary short arc rather than anything an enemy would walk
through.

`bot::look::Scan` holds the crosshair on one sight line for **1.2–3.4 s**, then
moves to another — usually the next one round, one time in four a jump to
another, because a perfectly cyclic sweep is its own tell. The motion between
targets is not animated here: the aim spring (`bot::aim`) turns the head, which
is what makes it read as a person rather than a lerp.

The dwell cap is not aesthetic. `DWELL_MAX` is below `IDLE_CHECK_INTERVAL`, so
a five-second idle window always contains at least one shift, and
`Scan::dwell_is_safe()` asserts it.

### Hearing

`svc_sound` was parsed and thrown away, with a comment saying gunfire was "a
genuine perception cue we will want later". It is now kept:

- `client::world::SoundEvent` records origin, volume, attenuation, entity and
  the `svc_time` it arrived at. The engine only sends a sound to clients inside
  its PAS (`SV_BuildSoundMsg`), **so receiving one is the audibility test** —
  there is no need to model walls.
- Loudness at the listener uses the engine's own falloff:
  `volume * (1 - distance * attenuation / SND_CLIP_DISTANCE)`.
- Our own entity's sounds are filtered out. A bot must not turn to look at its
  own footsteps.
- `world::Heard::urgency()` discounts loudness by age, so a shot half a second
  ago outranks a footstep now.

A sound above `HEAR_THRESHOLD` pulls the look toward it after a **0.15–0.38 s**
reaction delay (drawn per look, so two bots hearing the same shot do not turn in
lockstep), holds for 1.1–2.6 s, then the sweep resumes. A louder sound takes
over an existing one; a quieter one does not.

### And a smaller anti-idle

The drift amplitude drops from ~1.0° to ~0.3° — still three times the 0.1°
threshold with room for wire rounding, and `AntiIdle::guarantees()` still holds
at every phase (the margin now comes from the period ceiling rather than a big
amplitude).

## How to tell if it comes back

- `cargo test -p bot --lib look` — dwell length, reaction time, sound priority,
  and that two seeds do not move in step.
- `cargo test -p nav --lib watch` — every returned point is visible from the
  spot, inside the radius, and genuinely a different direction.
- Live: `cargo run -p client --example look_trace -- captures/<run>/<bot>.bin.sent`
  and look at **reversals/s**. Above ~2 while holding means something is
  driving the view every tick again.
- Live: `grep "heard:" captures/<run>/<bot>.log`. Permanently zero means the bot
  is deaf — check that `svc_sound` still decodes.

## Deliberately not done

- **Glancing at sounds while walking.** The steering axes are decomposed
  against the view actually sent, so a big head turn mid-route changes where
  the body goes. Doing it properly means bounding the glance against the travel
  bearing, and that deserves its own change with its own measurements.
- **Distinguishing sound types.** A footstep and a rifle are told apart only by
  loudness right now; the sound index is captured but not yet mapped to the
  precache list, which is what would let a bot treat a reload differently from
  a shot.

---

# Part two: they were never camping at all

Reported after the shake was fixed: *"when they camp they are looking at a wall,
not from where the players can come"*, and *"each time it goes mid and tries to
go to A ... it is doing loops"*.

Both turned out to be the same bug, and it is not a looking bug.

## What the sight lines were

`nav::watch` originally returned **the farthest walkable node with a clear line
of sight in each direction**. On the de_dust2 sites that is a fan of four
directions all round the room, most of which no attacker ever uses:

```text
site centre, old selection:  -99 deg 900u   147 deg 649u   -155 deg 606u   -57 deg 249u
same spot, now:             -110 deg 306u   -72 deg 557u
```

The two it gives now are the tunnel mouth and the door. The rule changed from
"what can I see" to **"where does an attacker coming from their spawn first come
into view"**: route from the enemy spawn to the spot, walk that route back out,
and keep the far end of the unbroken run of nodes still visible from here. Force
a different route (charge the nodes already used) and repeat. `cargo run -p
client --example watch_points` prints both selections side by side for any map.

Two details that cost a measurement each:

* The point where a route *enters* a site is usually **not visible** from inside
  it, so watching that is watching the wall in front of it.
* Taking the farthest visible node on the route rather than the end of the
  unbroken visible run picks a spot past a wall where the attacker flickers in
  and out — measured, that put a watch point 121 degrees off the route.

## Why it looked like a wall

The bots were **never holding a position at all**. Measured across six bots and
ten minutes: not one sample ever reached the holding branch.

The camp task walks to a defend point the caller picks 300–1700 units away.
Walking there took the bot outside the *objective's* arrival radius, so
`arrived` went false, the camp task stopped being ticked, the route pulled it
back to the objective, it arrived, and it set off again — the loop that was
reported as "it goes mid, then tries to go to A, in circles". The straight-line
walk also had no route behind it, so anything in the way was a wall to grind
into. What looked like camping was a bot mid-reposition, aiming at a floor point
several hundred units away with geometry in between.

Four changes, each with its own measurement:

| change | before | after |
| --- | --- | --- |
| The camp spot is the destination while a task exists (and is published as `nav_goal`, so the caller **routes** there) | `to_spot` median 613 u, never arrives | median **186 u**, arrived in **40%** of samples |
| Give-up budget scaled to the walk, not a flat 6 s | expired on every task | tasks survive to the hold |
| Sight lines refreshed every tick, not snapshotted at task creation | a third of camp samples had none | **0%** have none |
| Sound threshold 0.12 → 0.38 plus a 2.2 s glance cooldown | **every** holding sample had the head on a sound | glancing on 15% of holding samples |

Result, with all four in: a holding bot is within 20° of a real entrance in
**47%** of samples (median 28°), and the rest are mostly mid-swing between two
angles — a player crossing from one angle to the other is also "off both".
Walking bots read 60° off, which is correct: they look where they are going.

## One thing tried and reverted

Watching the entrances *while walking* to the spot. It reads well and it broke
the walking: the movement code stops the body when the head is more than
`TURN_STOP_ANGLE` off the direction of travel, so a crosshair parked on a
doorway 90 degrees away froze the bot in place. The revert is commented at the
call site so it is not tried again.

## Still open

- The hold is short (`camp_hold()` is 5–12 s scaled by fear) and the bot then
  hunts. Whether a defender should hold longer is a design question, not a bug.
- `sight_lines` (the old "what can I see" selection) is kept as the fallback for
  spots with no route to work from. It is the wrong answer, just better than no
  answer.
