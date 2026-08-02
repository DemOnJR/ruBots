//! Picking hostages up, walking them out, and not losing them on the way.
//!
//! ## Why this is a press, not a hold
//!
//! `CHostage::ObjectCaps` returns `... | FCAP_ONOFF_USE`
//! (`dlls/hostage/hostage.cpp:988`) — not `FCAP_CONTINUOUS_USE`, which is what
//! the bomb has. `CBasePlayer::PlayerUse` splits on exactly that:
//!
//! ```text
//! if (((pev->button & IN_USE)      && (caps & FCAP_CONTINUOUS_USE))
//!  || ((m_afButtonPressed & IN_USE) && (caps & (FCAP_IMPULSE_USE | FCAP_ONOFF_USE))))
//! ```
//! (`dlls/player.cpp:4413-4414`). So a hostage responds to the **rising edge**
//! of `IN_USE` and to nothing else. Holding the key does nothing at all after
//! the first frame.
//!
//! ## The trap nobody sees coming
//!
//! `FCAP_ONOFF_USE` also means the **release** calls `Use`:
//!
//! ```text
//! else if ((m_afButtonReleased & IN_USE) && (caps & FCAP_ONOFF_USE))
//!     pObject->Use(this, this, USE_SET, 0);
//! ```
//! (`dlls/player.cpp:4419-4425`). And `CHostage::Use` **ignores `useType` and
//! `value` entirely** — it toggles on whether the hostage is already following
//! (`dlls/hostage/hostage.cpp:881-925`). So press-then-release is
//! follow-then-unfollow, and the bot would spend the round picking a hostage up
//! and immediately putting it down.
//!
//! What saves it is the rate limit that is the next line of that function:
//!
//! ```text
//! if (gpGlobals->time >= m_flNextChange) {
//!     m_flNextChange = gpGlobals->time + 1.0f;
//! ```
//! (`dlls/hostage/hostage.cpp:906-908`). The press consumes the toggle and
//! locks the hostage for a second, so a release **inside that second** is
//! swallowed. Therefore: press for one tick, release on the next, and do not
//! touch it again for a second. Holding `IN_USE` down for more than a second
//! and then letting go is precisely the way to un-recruit a hostage.
//!
//! ## Following is not observable, so it is *believed* and then *checked*
//!
//! This is the thing that kept every rescue from ever happening. Nothing on the
//! wire says which hostage is following whom: the only hostage traffic a client
//! gets is `HostagePos`, a radar blip carrying an index and a position
//! (`hostage.cpp:1262-1268`), and `HostageK`, an index that means "this one is
//! gone" (`:1291-1293`). So [`crate::world::HostageView::following_me`] is
//! hard-wired `false` by the projection layer, and a machine that waits to be
//! told it succeeded waits forever.
//!
//! Before this was fixed the machine did exactly that: it stood inside use
//! range of the nearest hostage, saw `following_me == false` on every tick,
//! stayed in `Grabbing`, and emitted a rising edge **once a second, forever**.
//! Each edge toggles (`hostage.cpp:906-925`), so the hostage was recruited,
//! un-recruited, recruited, un-recruited — and the bot never once walked
//! toward a rescue zone.
//!
//! What replaces it: the machine keeps its own roster. A rising edge aimed at a
//! hostage adds it to [`HostageEscort::led`] on the assumption the press
//! landed, and the assumption is then **checked against what is observable** —
//! a hostage that is following *walks*, and a hostage that is not stays exactly
//! where it was left. `DoFollow` is the only thing that moves one and it does
//! nothing without `m_hTargetEnt` (`:1027-1035`), so a changed position is
//! proof somebody is leading it; and once it has caught up and stopped moving
//! it is inside the 80 units `DoFollow` stops closing at (`:1052`). Neither for
//! [`RECRUIT_PATIENCE`] seconds means the press did not take, the belief is
//! dropped, and the bot goes back for it. Self-correcting, and it costs one
//! walk back rather than a whole round.
//!
//! Getting that window wrong is expensive in a way that is easy to miss.
//! `HostagePos` is a **1 Hz** radar blip that is skipped entirely while the
//! hostage is standing still (`:522-539`), so a short patience declares an
//! obedient hostage lost, and the bot goes back and presses it again — which
//! **un-recruits it**. A live cs_italy run with the window at 2.5 s produced
//! 113 edges in 280 seconds and never left the room.
//!
//! ## Where to aim, which is not where the hostage is
//!
//! `PlayerUse` measures the two halves of "can I use this" against two
//! different points, and neither of them is the hostage's origin:
//!
//! * **Range** is from the player's **feet** to the object's **bounding box** —
//!   `UTIL_FindEntityInSphere(pObject, pev->origin, MAX_PLAYER_USE_RADIUS)`
//!   (`dlls/player.cpp:4366`), and the engine's `FindEntityInSphere` clamps the
//!   search origin into `absmin`/`absmax` per axis before measuring
//!   (`rehlds/engine/pr_cmds.cpp:872-882`). A hostage's hull is
//!   `(-10,-10,0)..(10,10,62)` (`dlls/hostage/hostage.h:39-40`), so its box
//!   reaches 62 units above the origin the radar reports.
//! * **The cone** is from the player's **eyes** to `VecBModelOrigin(pObject)`
//!   (`:4374`), which is `absmin + size * 0.5` (`dlls/bmodels.cpp:4-7`) — for a
//!   hostage, its origin plus 31 in z.
//!
//! Aiming at the origin therefore aims at the hostage's *feet*, and the closer
//! the bot gets the worse that is. Eyes at +17 looking down at a point on the
//! floor 30 units away is 29.5 degrees below horizontal; the server measures
//! against a point 31 units up, 25 degrees *above* horizontal. 54 degrees apart
//! is `flDot ~ 0.58`, and `VIEW_FIELD_NARROW` is 0.7 (`dlls/util.h:42`) — so
//! walking all the way up to a hostage and looking straight at it is a use the
//! server silently refuses. [`hostage_use_point`] is the point it actually
//! measures, and both the aim and the reach test use it.

use crate::math::{distance, dot, forward, normalize, sub, Angles, Vec3, VIEW_HEIGHT};
use crate::world::{HostageView, Team, WorldView};

use super::bomb::{USE_RADIUS, VIEW_FIELD_NARROW};

/// `m_flNextChange` cadence — one toggle per second
/// (`dlls/hostage/hostage.cpp:906-908`).
pub const HOSTAGE_TOGGLE_INTERVAL: f32 = 1.0;

/// Distance within which a following hostage stops closing
/// (`dlls/hostage/hostage.cpp:1052`).
pub const HOSTAGE_FOLLOW_SLACK: f32 = 80.0;

/// Distance past which a following hostage is flagged stuck
/// (`dlls/hostage/hostage.cpp:1102`).
pub const HOSTAGE_STUCK_DISTANCE: f32 = 200.0;

/// Hostage hull, `dlls/hostage/hostage.h:39-40`.
pub const HOSTAGE_HULL_MIN: Vec3 = [-10.0, -10.0, 0.0];
pub const HOSTAGE_HULL_MAX: Vec3 = [10.0, 10.0, 62.0];

/// How far the bot lets a hostage trail before slowing down.
///
/// **Chosen**, comfortably inside the 200-unit stuck threshold — waiting until
/// the hostage is *already* stuck is waiting too long.
pub const ESCORT_LEASH: f32 = 150.0;

/// Full running speed, and the walk speed that keeps a hostage in step.
///
/// The walk value is under `PM_UpdateStepSound`'s 150-unit silence threshold
/// (`pm_shared/pm_shared.cpp:395`), so slowing for the hostage is quiet as well
/// as considerate.
pub const ESCORT_WALK_SPEED: f32 = 130.0;

/// How long a believed recruit gets to prove it is following before the belief
/// is thrown away and the bot goes back for it.
///
/// **Chosen, and measured.** It has to be several radar periods: `HostagePos`
/// is republished at most once a second and only when the hostage has moved
/// more than a unit (`dlls/hostage/hostage.cpp:522-539`), so a short window
/// keeps declaring a perfectly obedient hostage lost simply because it has not
/// been reported yet. At 2.5 s a live cs_italy run dropped and re-pressed its
/// four hostages continuously -- 113 `+use` edges in 280 seconds, every one of
/// them a TOGGLE that un-recruited whatever it hit -- and the bot never left
/// the room. Four seconds is four radar periods, and the cost of being wrong is
/// one walk back from the leash distance.
pub const RECRUIT_PATIENCE: f32 = 4.0;

/// Distance at which a believed recruit is given up on immediately.
///
/// Past `CHostage`'s own stuck threshold with margin: at this range the server
/// has already flagged it and is five seconds from dropping the follow
/// entirely (`dlls/hostage/hostage.cpp:393`).
pub const RECRUIT_LOST_DISTANCE: f32 = 320.0;

/// How far off the way out the bot will step to collect another hostage.
///
/// **Chosen.** cs\_ maps cluster their four hostages in one room — on cs_italy
/// all four sit inside 150 units of each other — so a detour this short costs
/// almost nothing and quadruples the number of bodies that have to survive the
/// walk home for the round to be won.
pub const GRAB_DETOUR: f32 = 260.0;

/// How many hostages one bot will track. cs\_ maps ship four
/// (`MAX_HOSTAGE_ICON`, and every stock map agrees).
pub const MAX_TRACKED: usize = 8;

/// What a unit of height costs when choosing which hostage to walk to.
///
/// **Chosen, and a heuristic** — the escort has no navigation graph, so it
/// cannot ask for a real path length, and straight-line distance is a bad
/// proxy the moment two hostages are on different floors. cs_italy puts two of
/// its four at z 32 and the other two at z 160, in the same room: from the foot
/// of the stairs the upstairs pair are *nearer* in a straight line and much
/// further to walk to. Measured on a live run before this existed, a bot
/// standing at `[900 2248 36]` flipped between the two pairs every time its own
/// z changed by stepping, rerouted on every flip, and spent five minutes
/// oscillating 90 units from a hostage it never touched. Four is enough that a
/// full storey outweighs any horizontal gap inside one room.
pub const VERTICAL_PENALTY: f32 = 4.0;

/// How long the bot will keep walking at one hostage before trying another.
///
/// **Chosen.** Long enough to cross cs_italy (a CT spawn to the hostage house
/// is about 5900 units of route, some 25 seconds) with room to spare, short
/// enough that a hostage the navigation cannot actually reach does not cost the
/// whole round.
pub const APPROACH_TIMEOUT: f32 = 45.0;

/// How long a hostage that defeated the approach is left alone afterwards.
pub const APPROACH_AVOID_TIME: f32 = 30.0;

/// The point `CBasePlayer::PlayerUse` measures its cone against: the centre of
/// the hostage's bounding box, `VecBModelOrigin` = `absmin + size * 0.5`
/// (`dlls/bmodels.cpp:4-7`, `dlls/player.cpp:4374`).
pub fn hostage_use_point(origin: Vec3) -> Vec3 {
    [
        origin[0],
        origin[1],
        origin[2] + (HOSTAGE_HULL_MIN[2] + HOSTAGE_HULL_MAX[2]) * 0.5,
    ]
}

/// Distance from `from` to the hostage's axis-aligned box, the way the engine
/// measures it in `FindEntityInSphere` (`rehlds/engine/pr_cmds.cpp:872-882`):
/// per axis, zero inside the box, otherwise the overshoot.
pub fn distance_to_hostage_box(from: Vec3, origin: Vec3) -> f32 {
    let mut sum = 0.0f32;
    for axis in 0..3 {
        let lo = origin[axis] + HOSTAGE_HULL_MIN[axis];
        let hi = origin[axis] + HOSTAGE_HULL_MAX[axis];
        let d = if from[axis] < lo {
            from[axis] - lo
        } else if from[axis] > hi {
            from[axis] - hi
        } else {
            0.0
        };
        sum += d * d;
    }
    sum.sqrt()
}

/// Both halves of `PlayerUse`'s test for a hostage, each against the point the
/// server actually uses: range from the feet to the box, cone from the eyes to
/// the box centre.
pub fn hostage_in_reach(me: Vec3, view: Angles, hostage: Vec3) -> bool {
    if distance_to_hostage_box(me, hostage) > USE_RADIUS {
        return false;
    }
    let eye = [me[0], me[1], me[2] + VIEW_HEIGHT];
    let los = normalize(sub(hostage_use_point(hostage), eye));
    if los == [0.0; 3] {
        return false;
    }
    dot(forward(view), los) > VIEW_FIELD_NARROW
}

/// Where the escort is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscortPhase {
    /// Nothing to do — no hostages, or not a CT.
    Idle,
    /// Walking to a hostage that is not following yet.
    Approaching,
    /// In range and in the cone; issuing the one-tick `+use` press.
    Grabbing,
    /// Leading hostages toward a rescue zone.
    Leading,
    /// Every hostage we had is delivered.
    Delivered,
}

impl EscortPhase {
    /// A word for the logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Approaching => "approach",
            Self::Grabbing => "grab",
            Self::Leading => "lead",
            Self::Delivered => "delivered",
        }
    }
}

/// What the escort machine wants this tick.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct EscortOutput {
    /// Assert `IN_USE` this tick. Never true on two consecutive ticks.
    pub use_action: bool,
    pub look_at: Option<Vec3>,
    /// Walk toward this **now**. `None` means stand still.
    pub move_to: Option<Vec3>,
    /// Where the escort is headed, whether or not it is walking this tick.
    ///
    /// Separate from `move_to` because the caller's navigation layer routes to
    /// this, and a route is expensive to throw away. The two ticks that stand
    /// still -- pressing `+use`, and waiting for a hostage that has fallen past
    /// the stuck threshold -- must not read as "no destination", or the router
    /// falls back to the map's declared objective and recomputes the whole path
    /// twice a second. Measured on cs_italy: `reroutes` climbing on every grab,
    /// with the follower's stuck-nudge firing into the gaps.
    pub goal: Option<Vec3>,
    /// Slow down: a hostage is trailing, or we are being quiet.
    pub walk: bool,
}

/// A hostage this bot believes it recruited, and the evidence for it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Recruit {
    entity: u16,
    /// Seconds since it last did something a follower would do.
    unconfirmed: f32,
    /// Where it was last reported, so movement can be detected.
    last_origin: Vec3,
}

/// The hostage machine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HostageEscort {
    pub phase: EscortPhase,
    /// Which hostage we are trying to pick up.
    pub target: Option<u16>,
    /// `IN_USE` state last tick, for producing a genuine rising edge.
    pressed_last_tick: bool,
    /// Seconds until the hostage's `m_flNextChange` lock expires.
    cooldown: f32,
    /// Rising edges emitted, total. Diagnostic.
    pub edges: u32,
    /// Hostages believed to be following, pending confirmation.
    led: [Option<Recruit>; MAX_TRACKED],
    /// Seconds spent walking at [`Self::target`] without reaching it.
    approach_time: f32,
    /// Hostages the approach has already failed at, and the time left on that.
    avoid: [Option<(u16, f32)>; MAX_TRACKED],
    /// Hostages this bot recruited that then vanished from the world — killed,
    /// or delivered. Diagnostic; the two are not distinguishable from a client.
    pub lost: u32,
}

impl Default for HostageEscort {
    fn default() -> Self {
        Self {
            phase: EscortPhase::Idle,
            target: None,
            pressed_last_tick: false,
            cooldown: 0.0,
            edges: 0,
            led: [None; MAX_TRACKED],
            approach_time: 0.0,
            avoid: [None; MAX_TRACKED],
            lost: 0,
        }
    }
}

impl HostageEscort {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// True while a pickup or an escort is under way.
    pub fn is_busy(&self) -> bool {
        matches!(
            self.phase,
            EscortPhase::Approaching | EscortPhase::Grabbing | EscortPhase::Leading
        )
    }

    /// How many hostages the bot currently believes it is leading.
    pub fn led_count(&self) -> usize {
        self.led.iter().flatten().count()
    }

    /// True if `entity` is on the roster.
    fn is_led(&self, entity: u16) -> bool {
        self.led.iter().flatten().any(|r| r.entity == entity)
    }

    fn recruit(&mut self, entity: u16, origin: Vec3) {
        if self.is_led(entity) {
            return;
        }
        if let Some(slot) = self.led.iter_mut().find(|s| s.is_none()) {
            *slot = Some(Recruit { entity, unconfirmed: 0.0, last_origin: origin });
        }
    }

    /// Re-check every believed recruit against what the world reports.
    ///
    /// Three outcomes, and the middle one is the whole point of the roster:
    /// gone from the world (rescued or killed) drops it; still there and either
    /// close or closing renews it; still there and doing neither for
    /// [`RECRUIT_PATIENCE`] means the `+use` never landed.
    fn audit(&mut self, world: &WorldView, dt: f32) {
        let me = world.me.origin;
        let mut lost = 0;
        for slot in self.led.iter_mut() {
            let Some(r) = slot.as_mut() else { continue };
            let Some(h) = world
                .hostages
                .iter()
                .find(|h| h.entity == r.entity && h.alive && !h.rescued)
            else {
                // The index stopped being reported. `SendHostageEventMsg` fires
                // on a rescue as well as on a death (`hostage.cpp:471-482`),
                // and both arrive as the same `HostageK`, so this is "no longer
                // ours" and nothing finer.
                *slot = None;
                lost += 1;
                continue;
            };
            let d = distance(me, h.origin);
            // Two independent confirmations, and one of them is always
            // available. A hostage that MOVED is being led by somebody --
            // `DoFollow` is the only thing that walks one, and it does nothing
            // without `m_hTargetEnt` (`dlls/hostage/hostage.cpp:1027-1035`).
            // A hostage that has caught up and stopped does not move, but it is
            // then inside the 80 units `DoFollow` stops closing at (`:1052`).
            //
            // The one-unit threshold is the server's own: `SendHostagePositionMsg`
            // is skipped unless the hostage has moved more than a unit since the
            // last radar tick (`:526-534`), so anything smaller is measuring
            // rounding, not walking.
            let moved = distance(h.origin, r.last_origin) > 1.0;
            r.last_origin = h.origin;
            if moved || d <= HOSTAGE_FOLLOW_SLACK {
                r.unconfirmed = 0.0;
            } else {
                r.unconfirmed += dt;
            }
            if r.unconfirmed > RECRUIT_PATIENCE || d > RECRUIT_LOST_DISTANCE {
                *slot = None;
            }
        }
        self.lost += lost;
    }

    /// The nearest rescue zone, if the caller supplied any.
    fn nearest_zone(world: &WorldView, from: Vec3) -> Option<Vec3> {
        world
            .rescue_zones
            .iter()
            .copied()
            .min_by(|a, b| {
                distance(from, *a)
                    .partial_cmp(&distance(from, *b))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    }

    /// What it is worth walking to this hostage, in units-ish.
    ///
    /// Horizontal distance plus a heavy charge for height — see
    /// [`VERTICAL_PENALTY`] for why a straight line is the wrong measure here.
    fn approach_cost(from: Vec3, h: &HostageView) -> f32 {
        crate::math::distance2d(from, h.origin) + (h.origin[2] - from[2]).abs() * VERTICAL_PENALTY
    }

    fn is_avoided(&self, entity: u16) -> bool {
        self.avoid.iter().flatten().any(|(e, _)| *e == entity)
    }

    fn avoid_for(&mut self, entity: u16, seconds: f32) {
        if let Some(slot) = self
            .avoid
            .iter_mut()
            .find(|s| s.is_none_or(|(e, _)| e == entity))
        {
            *slot = Some((entity, seconds));
        }
    }

    /// The cheapest hostage that is not already ours and not being avoided.
    fn best_free<'a>(&self, world: &'a WorldView) -> Option<&'a HostageView> {
        let me = world.me.origin;
        world
            .live_hostages()
            .filter(|h| !h.following_me && !self.is_led(h.entity) && !self.is_avoided(h.entity))
            .min_by(|a, b| {
                Self::approach_cost(me, a)
                    .partial_cmp(&Self::approach_cost(me, b))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    }

    /// Which hostage to walk at, holding onto the last answer.
    ///
    /// The hysteresis is not a nicety. Re-deciding every tick means re-deciding
    /// on a `distance` that moves whenever the bot steps up a stair, and every
    /// change of mind reroutes the navigation layer from scratch. A bot doing
    /// that gets nowhere while looking perfectly busy.
    fn choose<'a>(&mut self, world: &'a WorldView, dt: f32) -> Option<&'a HostageView> {
        if let Some(current) = self.target {
            let still_there = world
                .live_hostages()
                .any(|h| h.entity == current && !h.following_me && !self.is_led(h.entity));
            if still_there && self.approach_time <= APPROACH_TIMEOUT {
                self.approach_time += dt;
                return world.live_hostages().find(|h| h.entity == current);
            }
            if still_there {
                // Walked at it for the whole of `APPROACH_TIMEOUT` and never
                // got into use range: whatever the route believes, this one is
                // not reachable from here. Leave it for somebody else.
                self.avoid_for(current, APPROACH_AVOID_TIME);
            }
        }
        self.approach_time = 0.0;
        let picked = self.best_free(world);
        self.target = picked.map(|h| h.entity);
        picked
    }

    /// How far the furthest of our hostages is trailing.
    fn trailing_distance(&self, world: &WorldView) -> Option<f32> {
        world
            .hostages
            .iter()
            .filter(|h| h.alive && !h.rescued && (h.following_me || self.is_led(h.entity)))
            .map(|h| distance(world.me.origin, h.origin))
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    }

    fn quiet(&mut self, phase: EscortPhase) -> EscortOutput {
        self.phase = phase;
        self.pressed_last_tick = false;
        EscortOutput::default()
    }

    /// Walk to `h` and, once the server would accept it, press once.
    fn pursue(&mut self, h: &HostageView, me: Vec3, view: Angles) -> EscortOutput {
        self.target = Some(h.entity);
        let aim = hostage_use_point(h.origin);

        if !hostage_in_reach(me, view, h.origin) {
            self.phase = EscortPhase::Approaching;
            self.pressed_last_tick = false;
            return EscortOutput {
                use_action: false,
                look_at: Some(aim),
                move_to: Some(h.origin),
                goal: Some(h.origin),
                walk: false,
            };
        }

        self.phase = EscortPhase::Grabbing;

        // A rising edge needs the previous tick to have been low, and the
        // hostage's own one-per-second lock has to have expired — pressing
        // inside it does nothing but risk the release toggling it back off.
        if self.pressed_last_tick || self.cooldown > 0.0 {
            self.pressed_last_tick = false;
            return EscortOutput {
                use_action: false,
                look_at: Some(aim),
                move_to: None,
                goal: Some(h.origin),
                walk: false,
            };
        }

        self.pressed_last_tick = true;
        self.cooldown = HOSTAGE_TOGGLE_INTERVAL;
        self.edges += 1;
        // Believe the press landed; `audit` is what makes that belief cheap to
        // be wrong about.
        self.recruit(h.entity, h.origin);
        EscortOutput {
            use_action: true,
            look_at: Some(aim),
            move_to: None,
            goal: Some(h.origin),
            walk: false,
        }
    }

    /// Advance one tick. `view` is where the bot is currently looking.
    pub fn tick(&mut self, world: &WorldView, view: Angles, dt: f32) -> EscortOutput {
        self.cooldown = (self.cooldown - dt).max(0.0);

        let me = &world.me;
        // `CHostage::Use` bails out for anyone who is not a CT
        // (dlls/hostage/hostage.cpp:895-904).
        if !me.alive || me.team != Team::CounterTerrorist {
            self.led = [None; MAX_TRACKED];
            let out = self.quiet(EscortPhase::Idle);
            self.target = None;
            self.approach_time = 0.0;
            return out;
        }

        self.audit(world, dt);
        for slot in self.avoid.iter_mut() {
            if let Some((_, left)) = slot.as_mut() {
                *left -= dt;
                if *left <= 0.0 {
                    *slot = None;
                }
            }
        }

        let leading = self.led_count() > 0 || world.my_hostages().next().is_some();
        let free = self.choose(world, dt).copied();

        // Nothing left to do.
        if !leading && free.is_none() {
            let phase = if self.lost > 0 || world.hostages.iter().any(|h| h.rescued) {
                EscortPhase::Delivered
            } else {
                EscortPhase::Idle
            };
            self.target = None;
            self.approach_time = 0.0;
            return self.quiet(phase);
        }

        // A hostage right here is worth collecting before walking out, and the
        // stock maps put all four in one room. Anything further away is a
        // detour, and the round is not long enough for detours.
        let detour = free.filter(|h| !leading || distance(me.origin, h.origin) <= GRAB_DETOUR);

        if let Some(h) = detour {
            return self.pursue(&h, me.origin, view);
        }

        if leading {
            let trailing = self.trailing_distance(world).unwrap_or(0.0);
            let zone = Self::nearest_zone(world, me.origin);

            // A hostage past the leash has to be waited for — past 200 units
            // the server gives up on it entirely.
            let slow = trailing > ESCORT_LEASH;
            let stalled = trailing > HOSTAGE_STUCK_DISTANCE;

            self.phase = EscortPhase::Leading;
            self.pressed_last_tick = false;
            self.target = None;
            self.approach_time = 0.0;
            return EscortOutput {
                use_action: false,
                look_at: zone,
                // Stop dead rather than drag them past the stuck threshold --
                // but stay routed there, so standing still costs nothing.
                move_to: if stalled { None } else { zone },
                goal: zone,
                walk: slow,
            };
        }

        self.quiet(EscortPhase::Idle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::aim_angles;
    use crate::world::SelfState;

    const HOSTAGE_AT: Vec3 = [100.0, 0.0, 0.0];
    const ZONE: Vec3 = [-2000.0, 0.0, 0.0];

    fn ct_world(at: Vec3, hostages: Vec<HostageView>) -> WorldView {
        WorldView {
            me: SelfState {
                origin: at,
                team: Team::CounterTerrorist,
                on_ground: true,
                ..Default::default()
            },
            hostages,
            rescue_zones: vec![ZONE],
            ..Default::default()
        }
    }

    fn hostage(entity: u16, at: Vec3, following: bool) -> HostageView {
        HostageView { entity, origin: at, following_me: following, ..Default::default() }
    }

    /// Aim at the point the server measures the cone against, not at the feet.
    fn look_at_hostage(from: Vec3, h: Vec3) -> Angles {
        aim_angles(crate::math::eye_position(from), hostage_use_point(h))
    }

    #[test]
    fn exactly_one_rising_edge_per_pickup_attempt() {
        // FCAP_ONOFF_USE: the hostage only reacts to the edge. Two consecutive
        // asserted ticks are one press as far as the server is concerned, and
        // the second one is wasted; worse, a long hold followed by a release is
        // an un-follow.
        let mut m = HostageEscort::default();
        let at: Vec3 = [140.0, 0.0, 0.0]; // 40 units from the hostage
        let w = ct_world(at, vec![hostage(1, HOSTAGE_AT, false)]);
        let view = look_at_hostage(at, HOSTAGE_AT);

        let mut prev = false;
        let mut edges = 0;
        for tick in 0..20 {
            let out = m.tick(&w, view, 0.05);
            assert!(!(out.use_action && prev), "held +use across tick {tick}");
            if out.use_action && !prev {
                edges += 1;
            }
            prev = out.use_action;
        }
        // 20 ticks * 50 ms = 1.0 s, so at most one toggle is even possible.
        assert_eq!(edges, 1, "expected exactly one usable edge in one second");
        assert_eq!(m.edges, 1);
    }

    /// The bug that made hostage rescue impossible.
    ///
    /// `following_me` is hard-wired `false` by the projection layer, because
    /// nothing on the wire carries it. The old machine therefore never left
    /// `Grabbing`: it pressed, saw no confirmation, waited out the one-second
    /// lock and pressed again — and every one of those presses toggles
    /// (`hostage.cpp:906-925`), so it recruited and un-recruited the same
    /// hostage until the round ended.
    #[test]
    fn a_press_is_believed_so_the_bot_stops_pressing_and_starts_walking() {
        let mut m = HostageEscort::default();
        let at: Vec3 = [140.0, 0.0, 0.0];
        let w = ct_world(at, vec![hostage(1, HOSTAGE_AT, false)]);
        let view = look_at_hostage(at, HOSTAGE_AT);

        // One press...
        let mut edges = 0;
        for _ in 0..4 {
            if m.tick(&w, view, 0.05).use_action {
                edges += 1;
            }
        }
        assert_eq!(edges, 1);
        assert_eq!(m.led_count(), 1, "the press is believed");

        // ...and from then on it leads, without ever pressing again. The
        // hostage is standing 40 units away, which is inside the slack
        // `DoFollow` stops closing at, so the belief keeps being renewed.
        for tick in 0..200 {
            let out = m.tick(&w, view, 0.05);
            assert!(!out.use_action, "re-pressed at tick {tick}: that un-follows it");
        }
        assert_eq!(m.phase, EscortPhase::Leading);
        assert_eq!(m.edges, 1);
    }

    /// ...but the belief is not free. A hostage that was never really recruited
    /// stays where it was left, and the bot has to notice and come back.
    #[test]
    fn a_press_that_did_not_land_is_detected_and_retried() {
        let mut m = HostageEscort::default();
        let hostage_at: Vec3 = [0.0, 0.0, 0.0];
        let mut at: Vec3 = [40.0, 0.0, 0.0];
        let mut w = ct_world(at, vec![hostage(1, hostage_at, false)]);

        // The press.
        for _ in 0..3 {
            m.tick(&w, look_at_hostage(at, hostage_at), 0.05);
        }
        assert_eq!(m.led_count(), 1);
        assert_eq!(m.phase, EscortPhase::Leading);

        // Now walk away toward the zone. The hostage does not move, because the
        // press never took.
        let mut dropped_at = None;
        for step in 0..80 {
            at[0] -= 12.5; // ~250 u/s at 20 Hz
            w.me.origin = at;
            m.tick(&w, look_at_hostage(at, hostage_at), 0.05);
            if m.led_count() == 0 && dropped_at.is_none() {
                dropped_at = Some(step);
                break;
            }
        }
        let step = dropped_at.expect("the belief must not survive a hostage that never moves");
        // Patience starts once it is outside the 80-unit slack, so the drop is
        // roughly (80 units of walking) + RECRUIT_PATIENCE.
        // (`step` comes from `0..80` with a break, so it cannot exceed 79 --
        // the `.expect()` above is what actually asserts. Kept as a bound on
        // how long the retry may take.)
        assert!(step < 79, "took {step} ticks to notice");
        // The audit runs at the top of the tick, so the same tick that drops
        // the belief already turns the bot round.
        assert_eq!(m.phase, EscortPhase::Approaching);
        let out = m.tick(&w, look_at_hostage(at, hostage_at), 0.05);
        assert_eq!(out.move_to, Some(hostage_at));
    }

    /// A hostage that really is following keeps up, and the belief must survive
    /// the whole walk home — including the radar's one-second silences.
    #[test]
    fn a_hostage_that_keeps_up_is_never_re_pressed() {
        let mut m = HostageEscort::default();
        let mut at: Vec3 = [40.0, 0.0, 0.0];
        let mut h: Vec3 = [0.0, 0.0, 0.0];
        let mut w = ct_world(at, vec![hostage(1, h, false)]);
        for _ in 0..3 {
            m.tick(&w, look_at_hostage(at, h), 0.05);
        }
        assert_eq!(m.led_count(), 1);

        for tick in 0..400 {
            at[0] -= 6.0;
            // The hostage trails by ~60 units, inside the slack, and its radar
            // blip only refreshes every second.
            if tick % 20 == 0 {
                h = [at[0] + 60.0, 0.0, 0.0];
            }
            w.me.origin = at;
            w.hostages[0].origin = h;
            let out = m.tick(&w, Angles::default(), 0.05);
            assert!(!out.use_action, "re-pressed a following hostage at tick {tick}");
            assert_eq!(m.led_count(), 1, "lost the belief at tick {tick}");
        }
        assert_eq!(m.phase, EscortPhase::Leading);
    }

    #[test]
    fn never_more_than_one_edge_per_second() {
        // `m_flNextChange = gpGlobals->time + 1.0f` — anything faster is a
        // no-op that only risks toggling the hostage back off. Four hostages in
        // one room is the case that can legitimately produce repeated edges.
        let mut m = HostageEscort::default();
        let at: Vec3 = [0.0, 0.0, 0.0];
        let w = ct_world(
            at,
            vec![
                hostage(1, [40.0, 0.0, 0.0], false),
                hostage(2, [-40.0, 0.0, 0.0], false),
                hostage(3, [0.0, 40.0, 0.0], false),
                hostage(4, [0.0, -40.0, 0.0], false),
            ],
        );

        let dt = 0.05;
        let mut edge_times = Vec::new();
        let mut t = 0.0f32;
        for _ in 0..200 {
            // 10 seconds. Look wherever the machine asks to look, which is what
            // the controller does.
            let view = m
                .target
                .and_then(|e| w.hostages.iter().find(|h| h.entity == e))
                .map(|h| look_at_hostage(at, h.origin))
                .unwrap_or_default();
            if m.tick(&w, view, dt).use_action {
                edge_times.push(t);
            }
            t += dt;
        }
        assert!(!edge_times.is_empty(), "it must actually try");
        for pair in edge_times.windows(2) {
            assert!(
                pair[1] - pair[0] >= HOSTAGE_TOGGLE_INTERVAL - 1e-4,
                "edges {} and {} are only {} apart",
                pair[0],
                pair[1],
                pair[1] - pair[0]
            );
        }
        assert!(edge_times.len() <= 10, "{} edges in 10 s", edge_times.len());
    }

    #[test]
    fn out_of_range_or_out_of_cone_it_walks_instead_of_pressing() {
        let mut m = HostageEscort::default();

        // Too far — MAX_PLAYER_USE_RADIUS is 64.
        let at: Vec3 = [400.0, 0.0, 0.0];
        let w = ct_world(at, vec![hostage(1, HOSTAGE_AT, false)]);
        let out = m.tick(&w, look_at_hostage(at, HOSTAGE_AT), 0.05);
        assert!(!out.use_action);
        assert_eq!(out.move_to, Some(HOSTAGE_AT));
        assert_eq!(m.phase, EscortPhase::Approaching);

        // In range but facing away — VIEW_FIELD_NARROW rejects it.
        let mut m = HostageEscort::default();
        let at: Vec3 = [140.0, 0.0, 0.0];
        let w = ct_world(at, vec![hostage(1, HOSTAGE_AT, false)]);
        let away = Angles { pitch: 0.0, yaw: 90.0 };
        assert!(!m.tick(&w, away, 0.05).use_action, "outside the +-45 degree cone");
    }

    /// The reach test has to agree with `PlayerUse`, which measures range to
    /// the hostage's box and the cone to the box's centre.
    #[test]
    fn standing_right_next_to_a_hostage_is_still_a_usable_position() {
        // Feet 30 units apart. Aiming at the ORIGIN puts the crosshair 29.5
        // degrees below horizontal while the server measures 25 degrees above
        // it: 54 degrees, flDot 0.58, refused. Aiming at the box centre is what
        // the server checks, so it must pass.
        let me: Vec3 = [30.0, 0.0, 0.0];
        let h: Vec3 = [0.0, 0.0, 0.0];

        let at_feet = aim_angles(crate::math::eye_position(me), h);
        assert!(
            !hostage_in_reach(me, at_feet, h),
            "aiming at the feet must read as refused, because it is"
        );

        let at_centre = look_at_hostage(me, h);
        assert!(
            hostage_in_reach(me, at_centre, h),
            "aiming where the server looks must be accepted"
        );

        // ...and the machine aims there.
        let mut m = HostageEscort::default();
        let w = ct_world(me, vec![hostage(1, h, false)]);
        let out = m.tick(&w, at_centre, 0.05);
        assert_eq!(out.look_at, Some(hostage_use_point(h)));
        assert!(out.use_action, "in range, in the cone, and the lock is clear");
    }

    /// Range is measured to the box, not to the origin, so a hostage whose feet
    /// are 70 units away is still reachable if its body is not.
    #[test]
    fn the_use_radius_is_measured_against_the_hostage_hull() {
        let h: Vec3 = [0.0, 0.0, 0.0];
        // 70 units along x: the box reaches out to x = 10, so the gap is 60.
        assert!((distance_to_hostage_box([70.0, 0.0, 0.0], h) - 60.0).abs() < 1e-3);
        // Directly above the box top.
        assert!((distance_to_hostage_box([0.0, 0.0, 100.0], h) - 38.0).abs() < 1e-3);
        // Inside it.
        assert_eq!(distance_to_hostage_box([0.0, 0.0, 30.0], h), 0.0);
    }

    #[test]
    fn a_terrorist_never_touches_a_hostage() {
        // dlls/hostage/hostage.cpp:895-904 — non-CTs get a hint message and
        // nothing else, so pressing is pure noise.
        let at: Vec3 = [140.0, 0.0, 0.0];
        let mut w = ct_world(at, vec![hostage(1, HOSTAGE_AT, false)]);
        w.me.team = Team::Terrorist;
        let mut m = HostageEscort::default();
        for _ in 0..40 {
            assert!(!m.tick(&w, look_at_hostage(at, HOSTAGE_AT), 0.05).use_action);
        }
        assert_eq!(m.edges, 0);
        assert_eq!(m.phase, EscortPhase::Idle);
    }

    #[test]
    fn once_following_it_leads_toward_the_rescue_zone() {
        let at: Vec3 = [100.0, 0.0, 0.0];
        // `following_me` reported by the world -- not observable on a real
        // server, but the machine must still honour it if it ever is.
        let w = ct_world(at, vec![hostage(1, [120.0, 0.0, 0.0], true)]);
        let mut m = HostageEscort::default();
        let out = m.tick(&w, Angles::default(), 0.05);
        assert_eq!(m.phase, EscortPhase::Leading);
        assert_eq!(out.move_to, Some(ZONE));
        assert!(!out.use_action, "never re-press a hostage that is already following");
        assert!(!out.walk, "20 units behind is well inside the leash");
    }

    #[test]
    fn a_trailing_hostage_slows_the_bot_down() {
        let at: Vec3 = [0.0, 0.0, 0.0];
        // 170 units back: past the leash, inside the stuck threshold.
        let w = ct_world(at, vec![hostage(1, [170.0, 0.0, 0.0], true)]);
        let mut m = HostageEscort::default();
        let out = m.tick(&w, Angles::default(), 0.05);
        assert!(out.walk, "should slow for a hostage past the leash");
        assert_eq!(out.move_to, Some(ZONE), "slow, but still heading out");
        assert!(ESCORT_WALK_SPEED < 150.0, "and quietly, under the footstep threshold");
    }

    #[test]
    fn a_hostage_past_the_stuck_threshold_makes_the_bot_wait() {
        let at: Vec3 = [0.0, 0.0, 0.0];
        // 250 units: dlls/hostage/hostage.cpp:1102 has already flagged it stuck.
        let w = ct_world(at, vec![hostage(1, [250.0, 0.0, 0.0], true)]);
        let mut m = HostageEscort::default();
        let out = m.tick(&w, Angles::default(), 0.05);
        assert_eq!(out.move_to, None, "stop and let it catch up");
        assert!(out.walk);
    }

    #[test]
    fn with_no_known_rescue_zone_it_still_does_not_wander() {
        // Rescue zones are not observable, so an empty list is normal. Leading
        // nowhere is better than leading somewhere invented.
        let at: Vec3 = [0.0, 0.0, 0.0];
        let mut w = ct_world(at, vec![hostage(1, [40.0, 0.0, 0.0], true)]);
        w.rescue_zones.clear();
        let mut m = HostageEscort::default();
        let out = m.tick(&w, Angles::default(), 0.05);
        assert_eq!(out.move_to, None);
        assert_eq!(out.look_at, None);
        assert!(!out.use_action);
    }

    #[test]
    fn dead_and_rescued_hostages_are_not_chased() {
        let at: Vec3 = [140.0, 0.0, 0.0];
        let mut m = HostageEscort::default();
        let w = ct_world(
            at,
            vec![
                HostageView { entity: 1, origin: HOSTAGE_AT, alive: false, ..Default::default() },
                HostageView { entity: 2, origin: HOSTAGE_AT, rescued: true, ..Default::default() },
            ],
        );
        let out = m.tick(&w, look_at_hostage(at, HOSTAGE_AT), 0.05);
        assert!(!out.use_action);
        assert_eq!(out.move_to, None);
        assert_eq!(m.phase, EscortPhase::Delivered, "one of them was delivered");
    }

    /// A recruit that stops being reported at all is gone -- and on a real
    /// server that is exactly what a rescue looks like, because
    /// `SendHostageEventMsg` fires for a rescue and a death alike.
    #[test]
    fn a_recruit_that_vanishes_counts_as_delivered() {
        let at: Vec3 = [40.0, 0.0, 0.0];
        let h: Vec3 = [0.0, 0.0, 0.0];
        let mut w = ct_world(at, vec![hostage(1, h, false)]);
        let mut m = HostageEscort::default();
        for _ in 0..3 {
            m.tick(&w, look_at_hostage(at, h), 0.05);
        }
        assert_eq!(m.led_count(), 1);

        w.hostages.clear();
        m.tick(&w, Angles::default(), 0.05);
        assert_eq!(m.led_count(), 0);
        assert_eq!(m.lost, 1);
        assert_eq!(m.phase, EscortPhase::Delivered);
    }

    #[test]
    fn the_nearest_free_hostage_is_the_one_chosen() {
        let at: Vec3 = [0.0, 0.0, 0.0];
        let w = ct_world(
            at,
            vec![hostage(1, [500.0, 0.0, 0.0], false), hostage(2, [120.0, 0.0, 0.0], false)],
        );
        let mut m = HostageEscort::default();
        m.tick(&w, Angles::default(), 0.05);
        assert_eq!(m.target, Some(2));
    }

    /// The one on this floor, not the one that happens to be nearer in a
    /// straight line and up a flight of stairs.
    ///
    /// cs_italy's four hostages sit at z 32 and z 160 in the same room, which
    /// is exactly this shape.
    #[test]
    fn a_hostage_one_floor_up_is_further_away_than_it_looks() {
        let at: Vec3 = [0.0, 0.0, 0.0];
        let w = ct_world(
            at,
            vec![
                hostage(1, [20.0, 88.0, 124.0], false),  // 90 in plan, a storey up
                hostage(2, [60.0, 104.0, -4.0], false),  // 120 in plan, same floor
            ],
        );
        let mut m = HostageEscort::default();
        m.tick(&w, Angles::default(), 0.05);
        assert_eq!(m.target, Some(2), "picked the one it would have to climb to");
    }

    /// ...and having picked, it does not change its mind every time it steps.
    ///
    /// Measured on cs_italy before the hysteresis: a bot at `[900 2248 36]`
    /// oscillating between z 36 and z 75 flipped its target on every step,
    /// rerouted the navigation layer on every flip, and stood there for the
    /// whole of a five-minute round 90 units from a hostage.
    #[test]
    fn stepping_up_a_stair_does_not_change_which_hostage_it_is_going_to() {
        // Chosen so the two costs genuinely INVERT as the bot's z bobs -- one
        // hostage on the bot's floor and slightly nearer, one a stair up and
        // slightly further. Without that inversion the test proves nothing
        // about hysteresis: it just restates the vertical penalty, and it
        // passes with the hysteresis deleted. (It did, and that was caught by
        // deleting it.)
        let near_low = hostage(1, [300.0, 0.0, 0.0], false);
        let far_high = hostage(2, [400.0, 0.0, 39.0], false);

        let cost = |from: Vec3, h: &HostageView| HostageEscort::approach_cost(from, h);
        let low = [0.0, 0.0, 0.0];
        let high = [0.0, 0.0, 39.0];
        assert!(
            cost(low, &near_low) < cost(low, &far_high),
            "on the low step the near one must win"
        );
        assert!(
            cost(high, &far_high) < cost(high, &near_low),
            "on the high step the far one must win -- otherwise there is nothing to resist"
        );

        let mut w = ct_world(low, vec![near_low, far_high]);
        let mut m = HostageEscort::default();
        m.tick(&w, Angles::default(), 0.05);
        let first = m.target;
        assert_eq!(first, Some(1), "should have started on the nearer one");

        for step in 0..200 {
            // The bot's own z bobs as it walks; nothing else changes.
            w.me.origin[2] = if step % 2 == 0 { 39.0 } else { 0.0 };
            m.tick(&w, Angles::default(), 0.05);
            assert_eq!(m.target, first, "changed its mind at step {step}");
        }
    }

    /// A hostage that is walking is being led, even when it is nowhere near us.
    ///
    /// This is the confirmation that keeps a real escort alive: the radar only
    /// speaks once a second, so "it is within 80 units right now" is not
    /// available on most ticks.
    #[test]
    fn a_recruit_that_keeps_moving_is_never_declared_lost() {
        let mut at: Vec3 = [40.0, 0.0, 0.0];
        let mut h: Vec3 = [0.0, 0.0, 0.0];
        let mut w = ct_world(at, vec![hostage(1, h, false)]);
        let mut m = HostageEscort::default();
        for _ in 0..3 {
            m.tick(&w, look_at_hostage(at, h), 0.05);
        }
        assert_eq!(m.led_count(), 1);

        // It trails at 130 units -- past the 80-unit slack for the whole run,
        // so proximity never confirms anything. Only the movement does.
        for tick in 0..400 {
            at[0] -= 5.0;
            if tick % 20 == 0 {
                h = [at[0] + 130.0, 0.0, 0.0];
            }
            w.me.origin = at;
            w.hostages[0].origin = h;
            m.tick(&w, Angles::default(), 0.05);
            assert_eq!(m.led_count(), 1, "declared lost at tick {tick}");
        }
    }

    /// Standing still is not the same as having nowhere to be.
    ///
    /// The router throws a whole path away when the goal changes, so the ticks
    /// that stop -- the `+use` press, and waiting for a hostage past the stuck
    /// threshold -- have to keep naming a destination.
    #[test]
    fn the_route_destination_survives_the_ticks_that_stand_still() {
        // Pressing.
        let me: Vec3 = [30.0, 0.0, 0.0];
        let h: Vec3 = [0.0, 0.0, 0.0];
        let w = ct_world(me, vec![hostage(1, h, false)]);
        let mut m = HostageEscort::default();
        let out = m.tick(&w, look_at_hostage(me, h), 0.05);
        assert!(out.use_action);
        assert_eq!(out.move_to, None, "stands still to press");
        assert_eq!(out.goal, Some(h), "but is still going somewhere");

        // Waiting for a stuck hostage.
        let w = ct_world([0.0, 0.0, 0.0], vec![hostage(1, [250.0, 0.0, 0.0], true)]);
        let mut m = HostageEscort::default();
        let out = m.tick(&w, Angles::default(), 0.05);
        assert_eq!(out.move_to, None);
        assert_eq!(out.goal, Some(ZONE));
    }

    /// A hostage the navigation cannot actually get to must not cost the round.
    #[test]
    fn a_hostage_it_cannot_reach_is_eventually_given_up_on() {
        // Two hostages, and the bot never moves -- so it never reaches either.
        let at: Vec3 = [0.0, 0.0, 0.0];
        let w = ct_world(
            at,
            vec![hostage(1, [900.0, 0.0, 0.0], false), hostage(2, [1200.0, 0.0, 0.0], false)],
        );
        let mut m = HostageEscort::default();
        m.tick(&w, Angles::default(), 0.05);
        assert_eq!(m.target, Some(1));

        let dt = 0.05;
        let ticks = ((APPROACH_TIMEOUT / dt) as usize) + 4;
        for _ in 0..ticks {
            m.tick(&w, Angles::default(), dt);
        }
        assert_eq!(m.target, Some(2), "still walking at the one it cannot reach");
        assert!(m.is_avoided(1));

        // And the skip expires, so a hostage is never written off for good.
        for _ in 0..((APPROACH_AVOID_TIME / dt) as usize + 4) {
            m.tick(&w, Angles::default(), dt);
        }
        assert!(!m.is_avoided(1));
    }

    /// Four hostages in one room: collect the ones that are right there before
    /// walking 6000 units home, but do not cross the map for a second one.
    #[test]
    fn a_hostage_within_arms_reach_is_collected_before_leaving() {
        let at: Vec3 = [40.0, 0.0, 0.0];
        let near: Vec3 = [0.0, 0.0, 0.0];
        let alongside: Vec3 = [40.0, 120.0, 0.0];
        let mut w = ct_world(at, vec![hostage(1, near, false), hostage(2, alongside, false)]);
        let mut m = HostageEscort::default();
        for _ in 0..3 {
            m.tick(&w, look_at_hostage(at, near), 0.05);
        }
        assert_eq!(m.led_count(), 1);

        // The second is 120 units away -- inside GRAB_DETOUR, so go and get it
        // rather than leaving it behind.
        let out = m.tick(&w, Angles::default(), 0.05);
        assert_eq!(m.phase, EscortPhase::Approaching);
        assert_eq!(out.move_to, Some(alongside));

        // Move it across the map and the answer flips: leave, and lead.
        w.hostages[1].origin = [3000.0, 0.0, 0.0];
        let out = m.tick(&w, Angles::default(), 0.05);
        assert_eq!(m.phase, EscortPhase::Leading);
        assert_eq!(out.move_to, Some(ZONE));
    }

    #[test]
    fn dying_mid_escort_clears_the_machine() {
        let at: Vec3 = [140.0, 0.0, 0.0];
        let mut w = ct_world(at, vec![hostage(1, HOSTAGE_AT, false)]);
        let mut m = HostageEscort::default();
        m.tick(&w, look_at_hostage(at, HOSTAGE_AT), 0.05);
        assert!(m.is_busy());
        w.me.alive = false;
        m.tick(&w, look_at_hostage(at, HOSTAGE_AT), 0.05);
        assert!(!m.is_busy());
        assert_eq!(m.target, None);
        assert_eq!(m.led_count(), 0, "a dead bot leads nobody: hostage.cpp:393");
    }
}
