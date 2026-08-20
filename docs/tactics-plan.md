# Tactical play plan: sites, lanes, rotation, post-plant

**Status:** planned (not started as a phase).  
**Scope:** bomb-defusal maps first (`de_dust2` freeze until humanize metrics pass,
then port anchors to other maps).  
**Audience:** make CT and T play read like competent matchmaking / low-pro defaults,
not “all run to one random objective and fight wherever.”

This sits **above** humanization W1–W7 and route diversity A2. Those make *individuals*
look human; this makes the **team** play a round correctly.

Related code today:

| Piece | Where | Gap |
|---|---|---|
| Roles Assault/Hold/Flank/Split | `crates/client/src/role.rs` | Seed-only site pick; **no CT default setup**, no mid-round reassignment |
| Approach rings | `role.rs` RING_* | Rings are generic; not **named lane anchors** (Long doors, Cat, Mid doors, B tunnels…) |
| Plant spots A/B | Phase D plant library | Plant OK; **no post-plant hold angles** for T |
| Bomb planted / position | decoder + `Decision.bomb_planted` | Present at plant for nearby clients; **late joiners may miss site**; no retake stack |
| Defuse rung | controller | Machine exists; **never observed live** (round timing + no CT priority race) |
| Teammate wipe / site loss | — | **No team state** → no rotation trigger |
| Lane info | PVS players only | No “heard mid / died Long → stack B” logic |

---

## 1. What “professional enough” means here

We are not cloning a full IGL callbook. We implement the **stable defaults** every
competent CS team uses on bomb maps:

### CT — priority stack (high → low)

1. **Do not leave both sites empty.** Default setup: bodies on A, B, and mid/lanes.
2. **Hold info lanes** that tell you where Ts go (mid, connector, long, tunnels).
3. **Rotate on evidence**, not on vibes: teammate wipe on a site, multi-contact on one
   path, or bomb carrier seen → stack that site; leave a lurk/delay on the other.
4. **After plant: retake is the only job.** Know which site, path in, clear angles,
   then **defuse** (kit buyers first when safe).
5. Save / eco behaviour later; not in v1.

### T — priority stack (high → low)

1. **Execute a site** with a role split (entry / trade / lurk / mid-control / fake).
2. **Plant** inside zone (already works with carrier override).
3. **After plant: get off the open plant and hold entry points** so CT cannot walk in
   free — crossfires on site entrances, not 5 bodies on the bomb.
4. Prevent defuse: kill kit, utility later; v1 = position + gunfights.
5. If the round is lost mid-way (site fail), mid-round re-hit / rotate is phase 2.

### Pro-default patterns to encode (not full demos)

Sources of truth for *structure* (not literal pro configs):

- **Default CT setups** on dust2-class maps: split A / mid / B with at least one
  body able to delay each site entry; mid is info + rotate, not a deathmatch island.
- **Rotation rules:** if two+ die on B with no plant yet → stack B; if plant goes A →
  everyone alive paths to A retake anchors (not random site centres).
- **Post-plant T:** “play the bomb” = hold **doors into site** (crossfire), one
  close/close-ish for defuse denial, not conga on the bomb model.
- **Post-plant CT:** trade space for time, enter from **two paths when numbers allow**,
  one player commits defuse under cover.

Map packs (v1 dust2 anchors, then YAML/table per map):

| Side | Anchor examples (dust2 names) | Purpose |
|---|---|---|
| CT | A long doors, A site car/goose, Cat, Mid doors, B car, B window/door, Tunnels CT | Default + info |
| T pre-plant | Long, Cat, Mid, B tunnels, Upper B | Execute paths |
| T post-plant A | Long doors hold, Cat hold, site dark / platform | Deny retake |
| T post-plant B | Tunnel exit, window, site back plat / car | Deny retake |
| CT retake A | Long + Cat + CT mid | Multi-path retake |
| CT retake B | CT mid + tunnels / window | Multi-path retake |

Exact world coords live in data (`tactics/de_dust2.toml` or extend plant-spot tables),
not hard-coded magic in the brain once the schema exists.

---

## 2. Phase map (G-series — tactics)

Order is deliberate: **shared team belief first**, then CT default, then rotation,
then post-plant both sides, then defuse race polish. Do not start with “smarter aim.”

### G0 — Team / round state bus (prerequisite)

**Problem:** each bot process only sees PVS + local bomb flags. Rotation needs
“B is dead” and “bomb is A.”

**Change:**

- Lightweight **fleet knowledge** on the existing telemetry channel (or a second
  localhost UDP port): each bot publishes every 0.5–1 s:
  - team, alive, origin, role, assigned site, rung
  - last damage/kill time if known
  - bomb: carried_by_me / planted / plant origin if known
- Each bot keeps a **TeamSnapshot** (same-team only): alive count per site sector,
  last contact sector, plant site enum `{Unknown,A,B}`.
- Plant site resolution: nearest bomb-site AABB to plant origin; if only
  `bomb_planted` with no origin, inherit from first teammate who reports a plant
  origin (fixes late-decode gap for retakes).

**Done when:** offline unit tests: 5 CT reports → wipe on B sector flips
`pressure=B`; plant at A coords → all CT snapshots say `PlantSite::A` within 1 s
of simulated messages.

### G1 — CT default setup (key spots + lanes)

**Problem:** CTs do not hold A/B/mid as a *setup*; roles still push generic rings.

**Change:**

- New CT roles (or map Hold/Flank/Split → anchors):
  - `SiteA`, `SiteB`, `Mid`, `Rotator` (starts mid/connector)
- Round-start assignment by seed **with team composition constraints** (via G0 or
  deterministic rank of steam/key hash so 10 CT ≈ 3A / 3B / 2 mid / 2 flex — scale
  with N).
- Destination = **named anchor** (cover node + look target down the lane), not
  plant-disc centre.
- CampTask (W5) aims down the lane (long doors, tunnels mouth, mid doors).

**Done when (live, de_dust2, N≥10 CT):**

| id | metric | target |
|---|---|---|
| CT-SETUP-1 | share of freezetime+15s CT samples in assigned sector AABBs | ≥ 70 % |
| CT-SETUP-2 | both sites have ≥1 live CT in first 20 s of round (when ≥4 CT alive) | ≥ 85 % of rounds |
| CT-SETUP-3 | mid/lane anchor occupied ≥1 CT when ≥6 CT alive | ≥ 70 % of rounds |

### G2 — CT rotation on site loss / contact

**Problem:** when B gets wiped, A stays camping A forever.

**Change:**

- Triggers (any):
  - same-team alive in sector B drops by ≥2 within 8 s, or sector alive == 0 while
    ≥1 enemy contact there (from kills/death telemetry or visible Ts)
  - bomb carrier / multiple Ts seen on path to B
- Response:
  - **Rotator + furthest safe site** path to threatened site retake/hold anchors
  - leave **1 delay** on the quiet site if ≥4 CT still alive (no full empty)
  - reassign `role` in logs: `rotate→B`
- Do **not** rotate all five on first shot mid (anti-fake): need 2 signals or a death.

**Done when:**

| id | metric | target |
|---|---|---|
| CT-ROT-1 | after simulated/logged B wipe, median time for ≥2 non-B CT to enter B approach | ≤ 12 s |
| CT-ROT-2 | opposite site still has ≥1 CT for ≥5 s after rotate starts (when ≥4 alive) | ≥ 60 % |
| CT-ROT-3 | false rotates (no contact, full stack wrong site) | ≤ 15 % of rounds |

### G3 — T execute stays role-based; fake optional later

Pre-plant T already has Assault/Hold/Flank/Split. Tighten:

- Assign **site commit** once per round (A or B) for majority; Split/Mid fakes stay
  mid for first 25 s then join commit if no contact.
- Entry order: Assault first into site, Hold trades, Flank secondary path.

**Done when:** pre-plant T site occupancy not 50/50 random noise; ≥60 % of T alive
share the commit site by 40 s if no plant yet.

### G4 — Post-plant T: defend the bomb (key holds)

**Problem:** after plant Ts wander or pile on the bomb; CT walks in free.

**Change:**

- On `bomb_planted` + T alive:
  - cancel long executes away from plant site
  - repath to **post-plant anchors** for that site (entry denial), not plant origin
  - roles: `Close` (1 bot, near bomb for defuse deny), `EntryHold` (2–3 on doors),
    `LurkExit` (1 on rotate path)
  - look targets = entrance vectors (into site), not floor at bomb

**Done when:**

| id | metric | target |
|---|---|---|
| T-PP-1 | median distance of non-dead T to bomb 10 s after plant | 200–700 u (not all &lt;100) |
| T-PP-2 | ≥2 distinct entrance anchors occupied when ≥3 T alive post-plant | ≥ 70 % |
| T-PP-3 | plant still happens ≥ baseline rate | no regression |

### G5 — Post-plant CT: retake + defuse priority

**Problem:** CTs do not race the bomb; defuse never observed.

**Change:**

- On plant site known:
  - all CT alive set goal to **retake anchors** for that site (multi-path when N≥3)
  - clear combat priority on site; then `defuse` rung when on bomb + safe enough
    (no visible T in X u, or commit with kit under smoke later)
- Kit: already bought by CTs with money — prefer kit holder as first defuser when
  two CTs on bomb.
- Round-time awareness if exposed: if C4 timer known/estimable, no mid camp.

**Structural test environment** (from RESUME defuse section):  
`mp_round_infinite` / forcerespawn / long c4timer scenario script so defuse is
*observable*; then normal rules.

**Done when:**

| id | metric | target |
|---|---|---|
| CT-PP-1 | share of rounds with plant where ≥1 CT reaches plant site AABB before boom | ≥ 50 % |
| CT-PP-2 | server log `Defused_The_Bomb` in scenario script | ≥ 1 per controlled run |
| CT-PP-3 | live 15v15: ≥1 defuse per 15 min when plants ≥5 (stretch) | track |

### G6 — Lane info layer (cheap “where are they?”)

**Problem:** rotate needs more than deaths.

**Change:**

- Sectorize dust2 (A long, A short/cat, mid, B tunnels, B site…).
- On visible enemy or teammate death, stamp **last_contact_sector** + time.
- Weight CT rotate and T mid-round rehit from contact age (&lt;15 s strong).

**Done when:** CT-ROT false-rotate rate drops vs G2 alone; logs show
`contact=mid` → mid-stack behaviour in tests.

### G7 — Map pack + pro reference notes

- `docs/tactics/de_dust2-anchors.md` — named spots + coords + look angles  
- Optional: short “why” notes from public default setups (long/cat/mid/B splits)  
- Schema reusable for `de_inferno`, `de_nuke`, … after dust2 humanize freeze lifts  

---

## 3. Interaction with the humanize freeze

| Work | Blocks tactics? |
|---|---|
| CONGA-1 / ROUTE-3 tuning (A2*) | Soft — rotates look worse in a conga, but G1 anchors **help** CONGA by splitting CT |
| Freeze jump fix | Required for readable setups |
| G0 team bus | Can land anytime; enables GUI “team belief” too |
| G1 CT anchors | **Can start in parallel** with A2; improves spectator quality immediately |
| G4/G5 post-plant | After G0 + reliable plant site; do not wait for CONGA-1 ≤45 % |

**Recommended order in the live loop:**

1. Keep A2* measure/improve for CONGA (short fires).  
2. **Implement G0 + G1 next as the tactics start** (user-visible CT defence).  
3. G2 rotation → G4 T post-plant → G5 CT retake/defuse → G6 info polish.  
4. G3 tighten T execute when defaults feel solid.

---

## 4. Telemetry / metrics additions

Extend `obj:` or brain lines:

```text
obj: rung hold role siteA sector long_doors contact mid plant none
obj: rung rotate role rot→B sector mid contact B plant none
obj: rung retake role entryHold sector b_door plant B
obj: rung defuse role close sector a_site plant A
```

New script section in `metrics.py` or `scripts/tactics_metrics.py`:

- CT-SETUP-*, CT-ROT-*, T-PP-*, CT-PP-* from above  
- Sector occupancy histograms for GUI Fleet tab  

---

## 5. Non-goals (v1 tactics)

- Perfect pixel lineups / jump-throws from demos (see utility plan U4)  
- Voice/radio callouts as strategy (W8 chat is cosmetic)  
- Perfect pro demos replay  
- Reading enemy through walls (only PVS + team bus + death events)

**Utility + economy** are tracked separately in **`docs/utility-economy-plan.md`**
(Phase U/E): slot ownership, no freezetime spam, eco/full buy classes.

---

## 6. Acceptance snapshot (dust2, 10v10 or 15v15, 15 min)

A tactics v1 drop is “good enough” when a spectator can say:

1. CTs **spawn into a setup** (A, mid, B) instead of a single site pile.  
2. When one site collapses, **others rotate** with someone delaying the empty side.  
3. After plant, Ts **hold doors**; CTs **come to the right site** and attempt defuse.  
4. Plants still happen; CONGA/PILE metrics do not collapse worse than pre-G1.

Humanize targets remain authoritative for movement; tactics targets above are
**additional** gates for Phase G.
