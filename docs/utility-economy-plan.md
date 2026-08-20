# Utility (nades) + economy plan

**Status:** design + partial code (Phase U / E).  
**Goal:** bots stop running with nades out doing nothing; buy and throw like a
competent CS 1.6 team — not “everyone HE at freezetime end into mid.”

Cross-links: `docs/tactics-plan.md` (G-series), `docs/RESUME-HERE.md`.

---

## 1. How pros actually use nades in 1.6 (what we copy)

CS 1.6 only has **HE / flash / smoke**. Pros do **not** dump util on round start.
Patterns that transfer to bots:

| Kind | When | Typical purpose (dust2-class maps) |
|---|---|---|
| **Smoke** | On **execute** or **retake**, once per choke | Block a cross (mid doors, CT, long corner) so entry is one-way |
| **Flash** | 0.5–1.5 s **before** a pop, or for **pop-flash** entry | Blind a known hold; never blind own team mid-corridor |
| **HE** | Stacked bodies / plant / defuse / eco rush | Damage through smoke / force off angle; not a first-second throw |

Rules we encode:

1. **One job per slot per round** — “mid doors smoke” is owned by **one** bot.
2. **No double-fill** — if that slot already fired (or is claimed), nobody else
   buys/throws a second mid doors smoke.
3. **Trigger, not timer** — freezetime never throws; throw when the **situation**
   matches (near choke for execute, post-plant, retake, visible stack).
4. **Gun first** — default weapon is rifle/pistol; nade only for the throw
   window (~1 s), then switch back. Fixes “running with nade in hand.”
5. **Eco saves util** — pistol/eco rounds buy 0–1 flash max, not full nade kits.

---

## 2. Architecture (practical for multi-process swarm)

Each bot is its own process. We **cannot** rely on shared memory for v1.

### 2.1 Deterministic slot ownership (no bus required)

Every utility **slot** has an id and a stable owner:

```text
owner = hash(slot_id) % team_size_bucket
```

In the swarm, bot seed / key hash maps to a team rank `0..N-1`. Only the owner
may **claim** that slot this round. Others never select that smoke.

This alone stops “10 smokes at mid doors.”

### 2.2 Optional team bus later (G0)

Broadcast on throw: `{ slot_id, kind, t }`. Peers mark slot **filled**.  
Until G0 lands, ownership is enough to prevent duplicates.

### 2.3 Lineup table (data, not AI magic)

Per map, a static table of lineups:

```text
slot_id | side | kind | from_xyz | aim_xyz | trigger | max_round_use=1
```

`from_xyz` = stand position (nav snap).  
`aim_xyz` = look target (simple lob; v1 is point-and-throw, not pixel lineups).  
v2 can add pitch offsets / run-throw flags from recorded demos.

### 2.4 Throw state machine (per bot)

```text
IdleGun → (slot armed & trigger true & in range of `from`)
       → PathToThrowSpot → AimAndPin → ReleaseThrow → SwitchToGun → Done(slot)
```

- `AimAndPin`: select nade, attack held 1 tick (1.6 pull pin), release attack to throw  
  (verify against `CBasePlayerWeapon` grenade path in ReGameDLL — pin/release).
- Abort if combat threat within X u (gunfight > nade).
- Abort if freezetime or dead.

### 2.5 Triggers (when)

| Trigger | Condition |
|---|---|
| `ExecuteChoke` | T, !planted, dist(me, choke) < R, round_time > freezetime+3s, site commit matches |
| `RetakeFlash` | CT, planted, pathing to plant site, dist < R |
| `PostPlantHE` | T, planted, enemy visible on site / defuse risk |
| `ContactHE` | visible enemy cluster / eco rush mid |
| `NeverAtT0` | hard ban: no util while freeze or t_alive < 3s after unfreeze |

### 2.6 dust2 v1 slot list (starter pack)

| slot_id | side | kind | purpose |
|---|---|---|---|
| `t_mid_doors_smoke` | T | smoke | Execute mid / cut CT |
| `t_xbox_flash` | T | flash | Mid pop |
| `t_long_smoke` | T | smoke | Long doors cut |
| `t_b_tunnels_flash` | T | flash | B entry |
| `t_site_he` | T | HE | Post-plant / stack |
| `ct_mid_smoke` | CT | smoke | Default mid deny |
| `ct_b_doors_flash` | CT | flash | B hold pop |
| `ct_retake_flash` | CT | flash | Post-plant retake |
| `ct_plant_he` | CT | HE | Clear plant / deny defuser |

Only **one** owner each. Assault role prefers execute smokes; Hold prefers CT
default smokes; etc. (soft preference on top of hash ownership).

---

## 3. Economy (pro-style buy system)

### 3.1 Round classes (per bot money; team money later via G0)

| Class | T money (approx) | CT money | Behaviour |
|---|---|---|---|
| **Pistol** | ≤ 800 | ≤ 800 | Armor if possible, no rifle, no util stack |
| **Eco / save** | &lt; 2000 | &lt; 2500 | Keep for next; maybe deagle **or** vest, **0 nades** |
| **Force** | 2000–3999 | 2500–4699 | Armor + SMG/cheap or half-buy; 0–1 flash |
| **Full** | ≥ 4000 | ≥ 4700 | Rifle + vesthelm + kit(CT) + 1–2 util by **slot ownership** |

Loss-bonus / team bank (G0): later refine with “team can full-buy if 3+ have ≥X.”

### 3.2 Buy priority (always)

1. VestHelm (or Vest on force)  
2. Rifle (AK/M4) if full/force allows  
3. Defuser (CT, full/force)  
4. **Only owned utility slots** this round (smoke > flash > HE for executes)  
5. Ammo  

Never: buy 2 smokes; never buy full util on eco.

### 3.3 Code home

- `crates/bot/src/economy.rs` — classify + plan  
- `Controller::buy_plan` delegates here  
- `crates/client/src/console.rs` `buy_plan` kept in sync or thin wrapper  

---

## 4. Phased delivery

| Phase | Deliverable | Done when |
|---|---|---|
| **E0** | Buy round classes + tests; no nade on eco | unit tests; freeze buys look sane live |
| **U0** | Never walk with nade out (force primary unless throwing) | live: 0 “nade-run” samples in brain log |
| **U1** | Slot table + ownership + claim API | offline: 10 CT seeds → ≤1 owner per slot |
| **U2** | Throw state machine + dust2 slots | **coded** (`ThrowMachine` pin/release); live measure pending |
| **U3** | Triggers (execute/retake/post-plant) | no mass T0 util; mid smoke ≤1/round |
| **E1** | Team eco via G0 bus | coordinated save rounds |
| **U4** | More lineups + HE on stacks | metrics UTIL-* |

### Metrics (add to harness later)

| id | meaning | target |
|---|---|---|
| UTIL-1 | throws during freezetime or first 2 s | **0** |
| UTIL-2 | max smokes same slot same round | **1** |
| UTIL-3 | share of live samples holding nade while `fwd>0` and not in throw | ≤ 2 % |
| ECO-1 | eco-class bots buying rifle | **0** |
| ECO-2 | full-buy bots with vesthelm+rifle when money allows | ≥ 90 % |

---

## 5. Non-goals v1

- Perfect pixel lineups / jump-throws from demos  
- One-ways and deep CT smokes with tick-perfect air time  
- Reading enemy util through walls  
- Buying util “because I can” without a slot  

---

## 6. Implementation notes (1.6)

- Buy aliases: `hegren`, `flash`, `sgren` (`Equipment`)  
- Select: `weapon_hegrenade` / `weapon_flashbang` / `weapon_smokegrenade`  
- Grenades are **Thrown** fire class — pin on attack press, throw on release  
  (confirm in `fire.rs` / ReGameDLL before U2 throw machine)  
- Do not leave `weapon_*grenade` selected after throw  

---

## 7. Relation to tactics G-series

| Depends on | Why |
|---|---|
| G1 anchors | throw `from` points co-located with holds |
| G4/G5 post-plant | HE/flash triggers |
| G0 bus | team eco + “slot filled” gossip |

Utility can start **U0+E0+U1 without G0**; U3 quality jumps with G0.
