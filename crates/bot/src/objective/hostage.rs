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
//! ## Escorting
//!
//! `CHostage::Think`'s follow logic stops closing the distance once it is
//! within 80 units of the leader (`dlls/hostage/hostage.cpp:1052`) and flags
//! itself stuck past 200 (`:1102`). So the leash is real: outrun it and the
//! hostage stops being escorted and starts being abandoned.

use crate::math::{distance, Angles, Vec3};
use crate::world::{HostageView, Team, WorldView};

use super::bomb::{can_use, USE_RADIUS, VIEW_FIELD_NARROW};

/// `m_flNextChange` cadence — one toggle per second
/// (`dlls/hostage/hostage.cpp:906-908`).
pub const HOSTAGE_TOGGLE_INTERVAL: f32 = 1.0;

/// Distance within which a following hostage stops closing
/// (`dlls/hostage/hostage.cpp:1052`).
pub const HOSTAGE_FOLLOW_SLACK: f32 = 80.0;

/// Distance past which a following hostage is flagged stuck
/// (`dlls/hostage/hostage.cpp:1102`).
pub const HOSTAGE_STUCK_DISTANCE: f32 = 200.0;

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

/// What the escort machine wants this tick.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct EscortOutput {
    /// Assert `IN_USE` this tick. Never true on two consecutive ticks.
    pub use_action: bool,
    pub look_at: Option<Vec3>,
    pub move_to: Option<Vec3>,
    /// Slow down: a hostage is trailing, or we are being quiet.
    pub walk: bool,
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
}

impl Default for HostageEscort {
    fn default() -> Self {
        Self {
            phase: EscortPhase::Idle,
            target: None,
            pressed_last_tick: false,
            cooldown: 0.0,
            edges: 0,
        }
    }
}

impl HostageEscort {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// True while a pickup or an escort is under way.
    pub fn is_busy(&self) -> bool {
        matches!(self.phase, EscortPhase::Approaching | EscortPhase::Grabbing | EscortPhase::Leading)
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

    /// The nearest hostage that is not already following us.
    fn nearest_free(world: &WorldView) -> Option<&HostageView> {
        world
            .live_hostages()
            .filter(|h| !h.following_me)
            .min_by(|a, b| {
                distance(world.me.origin, a.origin)
                    .partial_cmp(&distance(world.me.origin, b.origin))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    }

    /// How far the furthest of our hostages is trailing.
    fn trailing_distance(world: &WorldView) -> Option<f32> {
        world
            .my_hostages()
            .map(|h| distance(world.me.origin, h.origin))
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    }

    fn quiet(&mut self, phase: EscortPhase) -> EscortOutput {
        self.phase = phase;
        self.pressed_last_tick = false;
        EscortOutput::default()
    }

    /// Advance one tick. `view` is where the bot is currently looking.
    pub fn tick(&mut self, world: &WorldView, view: Angles, dt: f32) -> EscortOutput {
        self.cooldown = (self.cooldown - dt).max(0.0);

        let me = &world.me;
        // `CHostage::Use` bails out for anyone who is not a CT
        // (dlls/hostage/hostage.cpp:895-904).
        if !me.alive || me.team != Team::CounterTerrorist {
            let out = self.quiet(EscortPhase::Idle);
            self.target = None;
            return out;
        }

        let following: Vec<&HostageView> = world.my_hostages().collect();
        let free = Self::nearest_free(world);

        // Nothing left to do.
        if following.is_empty() && free.is_none() {
            let phase = if world.hostages.iter().any(|h| h.rescued) {
                EscortPhase::Delivered
            } else {
                EscortPhase::Idle
            };
            self.target = None;
            return self.quiet(phase);
        }

        // Somebody is following: lead them out. Picking up more is only worth
        // doing when one is right there, which the approach branch below
        // handles by distance.
        if !following.is_empty() {
            let trailing = Self::trailing_distance(world).unwrap_or(0.0);
            let zone = Self::nearest_zone(world, me.origin);

            // A hostage past the leash has to be waited for — past 200 units
            // the server gives up on it entirely.
            let slow = trailing > ESCORT_LEASH;
            let stalled = trailing > HOSTAGE_STUCK_DISTANCE;

            self.phase = EscortPhase::Leading;
            self.pressed_last_tick = false;
            return EscortOutput {
                use_action: false,
                look_at: zone,
                // Stop dead rather than drag them past the stuck threshold.
                move_to: if stalled { None } else { zone },
                walk: slow,
            };
        }

        // Nobody following yet: go and get one.
        let Some(h) = free else {
            return self.quiet(EscortPhase::Idle);
        };
        self.target = Some(h.entity);

        let in_reach = can_use(me.origin, view, h.origin, USE_RADIUS, VIEW_FIELD_NARROW);

        if !in_reach {
            self.phase = EscortPhase::Approaching;
            self.pressed_last_tick = false;
            return EscortOutput {
                use_action: false,
                look_at: Some(h.origin),
                move_to: Some(h.origin),
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
                look_at: Some(h.origin),
                move_to: None,
                walk: false,
            };
        }

        self.pressed_last_tick = true;
        self.cooldown = HOSTAGE_TOGGLE_INTERVAL;
        self.edges += 1;
        EscortOutput {
            use_action: true,
            look_at: Some(h.origin),
            move_to: None,
            walk: false,
        }
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

    #[test]
    fn exactly_one_rising_edge_per_pickup_attempt() {
        // FCAP_ONOFF_USE: the hostage only reacts to the edge. Two consecutive
        // asserted ticks are one press as far as the server is concerned, and
        // the second one is wasted; worse, a long hold followed by a release is
        // an un-follow.
        let mut m = HostageEscort::default();
        let at: Vec3 = [140.0, 0.0, 0.0]; // 40 units from the hostage
        let w = ct_world(at, vec![hostage(1, HOSTAGE_AT, false)]);
        let view = aim_angles(at, HOSTAGE_AT);

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

    #[test]
    fn never_more_than_one_edge_per_second() {
        // `m_flNextChange = gpGlobals->time + 1.0f` — anything faster is a
        // no-op that only risks toggling the hostage back off.
        let mut m = HostageEscort::default();
        let at: Vec3 = [140.0, 0.0, 0.0];
        let w = ct_world(at, vec![hostage(1, HOSTAGE_AT, false)]);
        let view = aim_angles(at, HOSTAGE_AT);

        let dt = 0.05;
        let mut edge_times = Vec::new();
        let mut t = 0.0f32;
        for _ in 0..200 {
            // 10 seconds
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
        let out = m.tick(&w, aim_angles(at, HOSTAGE_AT), 0.05);
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

    #[test]
    fn a_terrorist_never_touches_a_hostage() {
        // dlls/hostage/hostage.cpp:895-904 — non-CTs get a hint message and
        // nothing else, so pressing is pure noise.
        let at: Vec3 = [140.0, 0.0, 0.0];
        let mut w = ct_world(at, vec![hostage(1, HOSTAGE_AT, false)]);
        w.me.team = Team::Terrorist;
        let mut m = HostageEscort::default();
        for _ in 0..40 {
            assert!(!m.tick(&w, aim_angles(at, HOSTAGE_AT), 0.05).use_action);
        }
        assert_eq!(m.edges, 0);
        assert_eq!(m.phase, EscortPhase::Idle);
    }

    #[test]
    fn once_following_it_leads_toward_the_rescue_zone() {
        let at: Vec3 = [100.0, 0.0, 0.0];
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
        let out = m.tick(&w, aim_angles(at, HOSTAGE_AT), 0.05);
        assert!(!out.use_action);
        assert_eq!(out.move_to, None);
        assert_eq!(m.phase, EscortPhase::Delivered, "one of them was delivered");
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

    #[test]
    fn dying_mid_escort_clears_the_machine() {
        let at: Vec3 = [140.0, 0.0, 0.0];
        let mut w = ct_world(at, vec![hostage(1, HOSTAGE_AT, false)]);
        let mut m = HostageEscort::default();
        m.tick(&w, aim_angles(at, HOSTAGE_AT), 0.05);
        assert!(m.is_busy());
        w.me.alive = false;
        m.tick(&w, aim_angles(at, HOSTAGE_AT), 0.05);
        assert!(!m.is_busy());
        assert_eq!(m.target, None);
    }
}
