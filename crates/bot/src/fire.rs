//! Trigger discipline, closed on the server's own weapon state.
//!
//! The naive way to fire is to model the weapon: remember the cycle time, count
//! the ticks, decide when the next shot is due. That is a second implementation
//! of the server's timers, and it drifts. It does not need to exist, because
//! **the weapon's real state is on the wire**.
//!
//! ## What the server tells us
//!
//! `CBasePlayer::GetWeaponData` (`dlls/client.cpp:4975-4996`) packs the active
//! weapon into `weapon_data_t` every update. Two fields matter:
//!
//! * `m_flNextPrimaryAttack` — copied straight across (`client.cpp:4985`).
//! * `m_fInZoom` — which is **not zoom**: `item->m_fInZoom = weapon->m_iShotsFired`
//!   (`client.cpp:4990`). The field is reused, and it is how the bot sees its
//!   own spread building up.
//!
//! ## The timers are countdowns, not timestamps
//!
//! ReGameDLL is built with `CLIENT_WEAPONS`, so `UTIL_WeaponTimeBase()` returns
//! `0.0` (`dlls/util.cpp:29-35`) and `CBasePlayer::PostThink` decrements the
//! timers every frame:
//! `gun->m_flNextPrimaryAttack = Q_max(gun->m_flNextPrimaryAttack - gpGlobals->frametime, -1.0f)`
//! (`dlls/player.cpp:5450-5457`). The gate is then
//! `CanAttack(m_flNextPrimaryAttack, UTIL_WeaponTimeBase(), UseDecrement())`
//! (`dlls/weapons.cpp:1072`), i.e. `attack_time <= 0.0`.
//!
//! So `next_primary_attack <= 0.0` means **ready now**. It is not a timestamp,
//! it never needs to be compared against a clock, and nothing here has to know
//! any weapon's cycle time.
//!
//! ## Why holding the trigger is wrong for two of the three classes
//!
//! See [`crate::weapons`] — the fire class falls out of `m_iShotsFired`
//! handling. This module turns each class into a button pattern:
//!
//! * [`FireClass::SemiPistol`] — release between every shot, always.
//! * [`FireClass::FullAuto`] — hold until `shots_fired` reaches the burst cap,
//!   then release until it has decayed back down. Releasing is not politeness:
//!   the decay only happens in `ItemPostFrame`'s "no fire buttons down" branch
//!   (`dlls/weapons.cpp:1114`, decrement at `:1144-1146`), so a bot that never
//!   lets go never recovers its accuracy.
//! * [`FireClass::TimerOnly`] — hold; the weapon auto-repeats off the timer.

use crate::weapons::{fire_class, FireClass, WeaponId};

/// The active weapon as the server most recently described it.
///
/// Everything here comes from `weapon_data_t`; nothing is simulated.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WeaponState {
    pub id: WeaponId,
    /// `m_iClip`. `-1` for a weapon with no clip (`WEAPON_NOCLIP`).
    pub clip: i32,
    /// Reserve ammo for this weapon's ammo type.
    pub reserve: i32,
    /// `m_flNextPrimaryAttack` — a **countdown**. `<= 0.0` is ready.
    pub next_primary_attack: f32,
    /// `m_flNextSecondaryAttack` — likewise.
    pub next_secondary_attack: f32,
    /// `m_iShotsFired`, arriving in `weapon_data_t.m_fInZoom`
    /// (`dlls/client.cpp:4990`).
    pub shots_fired: i32,
    /// `m_fInReload`.
    pub in_reload: bool,
}

impl Default for WeaponState {
    fn default() -> Self {
        Self {
            id: WeaponId::None,
            clip: -1,
            reserve: 0,
            next_primary_attack: 0.0,
            next_secondary_attack: 0.0,
            shots_fired: 0,
            in_reload: false,
        }
    }
}

impl WeaponState {
    /// True when `ItemPostFrame` would let a primary attack through.
    pub fn primary_ready(&self) -> bool {
        self.next_primary_attack <= 0.0
    }

    pub fn secondary_ready(&self) -> bool {
        self.next_secondary_attack <= 0.0
    }

    /// True when the weapon has a clip and it is empty.
    pub fn clip_empty(&self) -> bool {
        self.clip == 0
    }

    /// True when reloading would achieve anything.
    pub fn can_reload(&self) -> bool {
        let max = crate::weapons::info(self.id).map_or(0, |w| w.clip);
        max > 0 && self.clip < max && self.reserve > 0 && !self.in_reload
    }

    pub fn fire_class(&self) -> FireClass {
        fire_class(self.id)
    }
}

/// Tunables for trigger discipline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FireParams {
    /// [`FireClass::FullAuto`] only: let go once `shots_fired` reaches this.
    ///
    /// The cost of a shot is `m_flAccuracy = shots³ / DIVISOR + 0.35`
    /// (`dlls/wpn_shared/wpn_ak47.cpp:97`), so the cube makes the penalty
    /// negligible for the first few and brutal after. With the AK's divisor of
    /// 200 (`dlls/weapons.h:706`), shot 4 costs `64/200 = 0.32` on top of the
    /// 0.35 floor; shot 8 costs `512/200 = 2.56` and saturates the 1.25 cap.
    /// **The number is chosen** — the curve it is chosen from is not.
    pub burst_shots: i32,
    /// Resume once `shots_fired` has decayed back to this or below.
    pub burst_resume_at: i32,
    /// Reload when the clip is at or below this and nothing is being shot at.
    pub reload_below: i32,
}

impl Default for FireParams {
    fn default() -> Self {
        Self { burst_shots: 4, burst_resume_at: 1, reload_below: 5 }
    }
}

/// What to do with the trigger and the reload key this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FireAction {
    pub attack: bool,
    pub reload: bool,
}

/// Why the trigger is not being pulled. Diagnostic — a bot that mysteriously
/// will not shoot is otherwise very hard to debug from the outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldFire {
    /// Nothing worth shooting at.
    NoTarget,
    /// Not a firing weapon (`WEAPON_NONE`, the shield, a grenade, the C4).
    NotAGun,
    /// `m_flNextPrimaryAttack` has not counted down yet.
    NotReady,
    /// Mid-reload.
    Reloading,
    /// Clip empty.
    NoAmmo,
    /// A pistol that fired last tick: `IN_ATTACK` must be released first.
    PistolMustRelease,
    /// Burst cap reached; waiting for `m_iShotsFired` to decay.
    BurstCooling,
}

/// Cross-tick trigger state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FireControl {
    pub params: FireParams,
    /// Whether `IN_ATTACK` was set on the previous tick.
    held_last_tick: bool,
    /// True between hitting the burst cap and `shots_fired` decaying back.
    cooling: bool,
    /// Why the last decision withheld the trigger, if it did.
    pub last_hold: Option<HoldFire>,
}

impl Default for FireControl {
    fn default() -> Self {
        Self {
            params: FireParams::default(),
            held_last_tick: false,
            cooling: false,
            last_hold: Some(HoldFire::NoTarget),
        }
    }
}

impl FireControl {
    pub fn new(params: FireParams) -> Self {
        Self { params, ..Self::default() }
    }

    /// Reset on death or weapon change — the latches describe a specific gun.
    pub fn reset(&mut self) {
        self.held_last_tick = false;
        self.cooling = false;
        self.last_hold = Some(HoldFire::NoTarget);
    }

    /// True when `IN_ATTACK` was set last tick.
    pub fn was_holding(&self) -> bool {
        self.held_last_tick
    }

    /// Decide the trigger for this tick.
    ///
    /// `want` is the engagement layer's answer to "is there something I want to
    /// shoot, and am I pointing at it". Everything else here is the hardware
    /// question: *may* I, and *should* I right now.
    pub fn decide(&mut self, w: &WeaponState, want: bool) -> FireAction {
        let action = self.decide_inner(w, want);
        self.held_last_tick = action.attack;
        action
    }

    fn hold(&mut self, why: HoldFire, reload: bool) -> FireAction {
        self.last_hold = Some(why);
        FireAction { attack: false, reload }
    }

    fn decide_inner(&mut self, w: &WeaponState, want: bool) -> FireAction {
        let class = w.fire_class();

        // A weapon that does not shoot never gets IN_ATTACK from here. The C4
        // and the grenades have their own machines, precisely because holding
        // IN_ATTACK means something completely different for them.
        if !matches!(
            class,
            FireClass::SemiPistol | FireClass::FullAuto | FireClass::TimerOnly | FireClass::Melee
        ) {
            self.cooling = false;
            return self.hold(HoldFire::NotAGun, false);
        }

        if w.in_reload {
            self.cooling = false;
            return self.hold(HoldFire::Reloading, false);
        }

        // Empty clip: reload rather than clicking. Melee has no clip.
        if w.clip_empty() && class != FireClass::Melee {
            self.cooling = false;
            return self.hold(HoldFire::NoAmmo, w.can_reload());
        }

        if !want {
            self.cooling = false;
            // Quiet moment: top the magazine up if it is worth doing.
            let reload = w.clip >= 0 && w.clip <= self.params.reload_below && w.can_reload();
            return self.hold(HoldFire::NoTarget, reload);
        }

        // The gate the server itself applies (`dlls/weapons.cpp:1072`).
        if !w.primary_ready() {
            return self.hold(HoldFire::NotReady, false);
        }

        match class {
            // `if (++m_iShotsFired > 1) return;`, reset only in the
            // no-buttons-down branch. One shot per press, no exceptions.
            FireClass::SemiPistol => {
                if self.held_last_tick {
                    self.hold(HoldFire::PistolMustRelease, false)
                } else {
                    self.last_hold = None;
                    FireAction { attack: true, reload: false }
                }
            }

            FireClass::FullAuto => {
                if self.cooling {
                    if w.shots_fired <= self.params.burst_resume_at {
                        self.cooling = false;
                    } else {
                        // Must be released for the decay to run at all.
                        return self.hold(HoldFire::BurstCooling, false);
                    }
                }
                if w.shots_fired >= self.params.burst_shots {
                    self.cooling = true;
                    return self.hold(HoldFire::BurstCooling, false);
                }
                self.last_hold = None;
                FireAction { attack: true, reload: false }
            }

            // Never touches m_iShotsFired; holding is correct and auto-repeats.
            FireClass::TimerOnly | FireClass::Melee => {
                self.last_hold = None;
                FireAction { attack: true, reload: false }
            }

            _ => self.hold(HoldFire::NotAGun, false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gun(id: WeaponId, clip: i32) -> WeaponState {
        WeaponState { id, clip, reserve: 90, ..Default::default() }
    }

    #[test]
    fn a_pistol_never_emits_two_consecutive_attack_ticks() {
        // The single most important rule in this file: holding IN_ATTACK on a
        // pistol fires exactly one bullet, ever.
        for id in [
            WeaponId::Usp,
            WeaponId::Glock18,
            WeaponId::P228,
            WeaponId::Deagle,
            WeaponId::Elite,
            WeaponId::FiveSeven,
        ] {
            let mut fc = FireControl::default();
            let w = gun(id, 12);
            let mut prev = false;
            let mut shots = 0;
            for tick in 0..200 {
                let a = fc.decide(&w, true);
                assert!(!(a.attack && prev), "{id:?} held the trigger at tick {tick}");
                if a.attack {
                    shots += 1;
                }
                prev = a.attack;
            }
            // It must still actually shoot — alternating, so about half.
            assert!(shots > 90, "{id:?} only fired {shots} times in 200 ticks");
        }
    }

    #[test]
    fn a_full_auto_releases_at_the_burst_cap_and_resumes_only_after_decay() {
        let mut fc = FireControl::new(FireParams {
            burst_shots: 4,
            burst_resume_at: 1,
            reload_below: 5,
        });
        let mut w = gun(WeaponId::Ak47, 30);

        // Four shots go out while shots_fired is under the cap.
        for expected in 0..4 {
            w.shots_fired = expected;
            assert!(fc.decide(&w, true).attack, "shot {expected} should be allowed");
        }

        // At the cap the trigger comes off, and stays off while shots_fired
        // sits there — which is the only way the server ever decays it
        // (dlls/weapons.cpp:1114).
        w.shots_fired = 4;
        assert!(!fc.decide(&w, true).attack, "must release at the cap");
        assert_eq!(fc.last_hold, Some(HoldFire::BurstCooling));
        for held in [4, 4, 3, 3, 2, 2] {
            w.shots_fired = held;
            assert!(!fc.decide(&w, true).attack, "still cooling at {held}");
        }

        // Only once the observed counter has come back down.
        w.shots_fired = 1;
        assert!(fc.decide(&w, true).attack, "should resume at the resume threshold");
    }

    #[test]
    fn the_burst_cap_is_not_a_tick_count_but_an_observed_one() {
        // If shots_fired never moves (the server never confirmed the shots),
        // the bot must not keep firing on a private counter.
        let mut fc = FireControl::default();
        let mut w = gun(WeaponId::M4a1, 30);
        w.shots_fired = 9; // already way over any cap
        for _ in 0..20 {
            assert!(!fc.decide(&w, true).attack, "over the cap, must not fire");
        }
    }

    #[test]
    fn nothing_fires_while_the_next_attack_countdown_is_positive() {
        for id in [WeaponId::Ak47, WeaponId::Usp, WeaponId::Awp, WeaponId::Knife] {
            let mut fc = FireControl::default();
            let mut w = gun(id, 10);
            w.next_primary_attack = 0.12;
            for _ in 0..10 {
                let a = fc.decide(&w, true);
                assert!(!a.attack, "{id:?} fired while not ready");
                assert_eq!(fc.last_hold, Some(HoldFire::NotReady));
            }
            // The instant it reaches zero it is allowed (`attack_time <= 0.0`).
            w.next_primary_attack = 0.0;
            assert!(fc.decide(&w, true).attack, "{id:?} should fire at exactly 0");
            // And a negative countdown is the normal resting state.
            w.next_primary_attack = -0.001;
            let mut fc2 = FireControl::default();
            assert!(fc2.decide(&w, true).attack);
        }
    }

    #[test]
    fn a_timer_only_weapon_may_simply_be_held() {
        // awp, scout, g3sg1, sg550, m3, xm1014 never touch m_iShotsFired, so
        // there is nothing to release for.
        for id in [
            WeaponId::Awp,
            WeaponId::Scout,
            WeaponId::G3sg1,
            WeaponId::Sg550,
            WeaponId::M3,
            WeaponId::Xm1014,
        ] {
            let mut fc = FireControl::default();
            let mut w = gun(id, 10);
            w.shots_fired = 30; // meaningless for these; must be ignored
            for _ in 0..20 {
                assert!(fc.decide(&w, true).attack, "{id:?} should hold the trigger");
            }
        }
    }

    #[test]
    fn nothing_fires_without_a_target() {
        let mut fc = FireControl::default();
        let w = gun(WeaponId::Ak47, 30);
        for _ in 0..10 {
            let a = fc.decide(&w, false);
            assert!(!a.attack);
            assert_eq!(fc.last_hold, Some(HoldFire::NoTarget));
        }
    }

    #[test]
    fn an_empty_clip_reloads_instead_of_clicking() {
        let mut fc = FireControl::default();
        let w = WeaponState { clip: 0, reserve: 60, ..gun(WeaponId::Ak47, 0) };
        let a = fc.decide(&w, true);
        assert!(!a.attack);
        assert!(a.reload);
        assert_eq!(fc.last_hold, Some(HoldFire::NoAmmo));

        // With no reserve there is nothing to reload either — do nothing.
        let dry = WeaponState { clip: 0, reserve: 0, ..gun(WeaponId::Ak47, 0) };
        let mut fc = FireControl::default();
        let a = fc.decide(&dry, true);
        assert!(!a.attack && !a.reload);
    }

    #[test]
    fn a_low_clip_is_topped_up_only_when_nothing_is_being_shot_at() {
        let mut fc = FireControl::default();
        let w = WeaponState { clip: 3, reserve: 60, ..gun(WeaponId::Ak47, 3) };
        assert!(fc.decide(&w, false).reload, "quiet moment: reload");

        let mut fc = FireControl::default();
        let a = fc.decide(&w, true);
        assert!(a.attack, "an enemy in front of you outranks a tidy magazine");
        assert!(!a.reload);
    }

    #[test]
    fn a_reloading_weapon_does_not_fire() {
        let mut fc = FireControl::default();
        let w = WeaponState { in_reload: true, ..gun(WeaponId::Ak47, 10) };
        let a = fc.decide(&w, true);
        assert!(!a.attack && !a.reload);
        assert_eq!(fc.last_hold, Some(HoldFire::Reloading));
    }

    #[test]
    fn non_guns_never_get_the_trigger() {
        // The C4 and the grenades absolutely must not be fired by this code —
        // IN_ATTACK on a C4 starts arming it, and on a grenade it pulls a pin.
        for id in [
            WeaponId::C4,
            WeaponId::HeGrenade,
            WeaponId::Flashbang,
            WeaponId::SmokeGrenade,
            WeaponId::None,
            WeaponId::ShieldGun,
        ] {
            let mut fc = FireControl::default();
            let w = gun(id, -1);
            assert!(!fc.decide(&w, true).attack, "{id:?} must not be fired");
            assert_eq!(fc.last_hold, Some(HoldFire::NotAGun));
        }
    }

    #[test]
    fn the_knife_has_no_clip_and_still_swings() {
        let mut fc = FireControl::default();
        let w = WeaponState { clip: -1, reserve: 0, ..gun(WeaponId::Knife, -1) };
        assert!(fc.decide(&w, true).attack);
    }

    #[test]
    fn ready_reads_the_countdown_not_a_clock() {
        let mut w = WeaponState::default();
        w.next_primary_attack = 0.0;
        assert!(w.primary_ready(), "0 is ready — CanAttack is `attack_time <= 0`");
        w.next_primary_attack = -1.0;
        assert!(w.primary_ready(), "-1.0 is the floor PostThink clamps to");
        w.next_primary_attack = 0.001;
        assert!(!w.primary_ready());
    }

    #[test]
    fn resetting_clears_the_latches() {
        let mut fc = FireControl::default();
        let w = gun(WeaponId::Usp, 12);
        assert!(fc.decide(&w, true).attack);
        assert!(fc.was_holding());
        fc.reset();
        assert!(!fc.was_holding());
        // ...so the first shot after a respawn is not eaten.
        assert!(fc.decide(&w, true).attack);
    }
}
