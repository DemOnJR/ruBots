//! Target selection and fire control — the "kill" half of the AI.
//!
//! Port of `findTarget` / `engage` / `fireButtons` from `internal/bot/think.go`.
//!
//! ## What is verified
//!
//! * `findTarget` (`0x140700A00`) reads each candidate's `number`,
//!   `origin[0..2]` and `view_ofs[2]`, and logs
//!   `"enemy spotted: player %d at %.0fu"` — so selection is distance-driven
//!   over living enemies.
//! * `engage` (`0x140701EA0`) calls [`crate::rng::chance`] four times and
//!   `rnd` five times: the engagement is deliberately randomised rather than
//!   deterministic.
//! * `fireButtons` (`0x140702CE0`) calls `rnd` twice and sets button bits —
//!   trigger discipline is randomised (tap/burst), not held down.
//! * The float pool `engage` compares against is recovered exactly; see
//!   [`RECOVERED_ENGAGE_CONSTANTS`].
//!
//! ## What is not
//!
//! The *role* of each recovered constant inside `engage`'s branch tree was not
//! established — `340.0` alone is compared in five different places. Rather
//! than invent meanings, the constants are recorded verbatim below and the
//! logic here is built only from the parts that are actually pinned down.
//! [`EngageParams`] is therefore tunable, with defaults drawn from the
//! recovered pool where the role is unambiguous.

//!
//! ## Correction: the health term is dead on a real server
//!
//! The original scoring here preferred a wounded enemy as a tie-break. On a
//! default server that term can never fire, because **other players' health is
//! not on the wire** — `entity_state_t` carries no health for anyone but
//! yourself. Every enemy reports the same value, so the term is identically 1.
//!
//! It is kept, at [`EngageParams::health_weight`], for the modded and
//! spectator-feed cases where health *is* available, but the scoring the bot
//! actually runs on is the three things it can observe: **distance**,
//! **visibility** (a hard filter — an enemy you cannot see is a navigation
//! problem, not a shooting one) and **whether they are already pointing at
//! you**, which is the one that decides who dies first in a real fight.

use crate::aim::aim_error;
use crate::math::{aim_angles, cos_degrees, distance, eye_position, Angles};
use crate::rng::Rng;
use crate::world::{PlayerView, WorldView};

/// Every float `engage` compares against, recovered from `.rdata`.
///
/// Recorded for future work: these are known to be the operands, but not yet
/// which branch each governs.
pub const RECOVERED_ENGAGE_CONSTANTS: [f64; 12] = [
    2.2, 7.0, 30.0, 90.0, 150.0, 250.0, 340.0, 500.0, 600.0, 900.0, 1000.0, 1200.0,
];

/// Tunables for engagement behaviour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EngageParams {
    /// Beyond this the bot will not open fire at all.
    pub max_engage_distance: f32,
    /// Aim must be within this many degrees before the trigger is pulled.
    pub fire_cone_degrees: f64,
    /// Probability of firing on any given tick once on target (trigger
    /// discipline — `fireButtons` randomises this rather than holding).
    pub fire_chance: f64,
    /// Below this distance the bot closes rather than holds.
    pub close_quarters_distance: f32,
    /// Half-angle, in degrees, within which an enemy counts as aiming at us.
    ///
    /// **Chosen.** ReGameDLL's own "roughly pointing at it" cone is
    /// `VIEW_FIELD_NARROW`, +-45 degrees (`dlls/util.h:42`); this is tighter,
    /// because "in my general direction" and "about to shoot me" are different
    /// claims.
    pub aiming_at_me_cone_degrees: f64,
    /// How much closer an enemy who is aiming at us is treated as being.
    ///
    /// Multiplies the effective distance, so `0.5` means "deal with them as if
    /// they were half as far away". **Chosen.**
    pub aiming_at_me_factor: f32,
    /// How much observed health may shift the effective distance, as a
    /// fraction. Inert on a default server — see the module docs. `0.25` keeps
    /// the original behaviour where health is genuinely available.
    pub health_weight: f32,
}

impl Default for EngageParams {
    fn default() -> Self {
        Self {
            // 1200 is the largest distance-like constant in engage's pool.
            max_engage_distance: 1200.0,
            fire_cone_degrees: 7.0,
            fire_chance: 0.75,
            close_quarters_distance: 150.0,
            aiming_at_me_cone_degrees: 20.0,
            aiming_at_me_factor: 0.5,
            health_weight: 0.25,
        }
    }
}

/// How desirable a target is — lower is better.
///
/// An effective distance, in units. Every term is multiplicative on the real
/// distance, which preserves the property that matters: **distance dominates**,
/// and no modifier can make a far enemy beat a much closer one. That is what
/// `findTarget`'s `"enemy spotted: player %d at %.0fu"` logging implies the
/// original does, and it is also just correct — the one shooting at you from
/// three feet away is the problem.
pub fn threat_score(world: &WorldView, p: &PlayerView, params: &EngageParams) -> f32 {
    let d = distance(world.me.origin, p.origin);

    // Health: inert on a default server, where every enemy reads the same.
    let w = params.health_weight.clamp(0.0, 1.0);
    let health = (1.0 - w) + w * (p.health.clamp(0.0, 100.0) / 100.0);

    // Aiming at us: the one term that is both observable and decisive.
    let cone = cos_degrees(params.aiming_at_me_cone_degrees);
    let aiming = if p.is_aiming_at(eye_position(world.me.origin), cone) {
        params.aiming_at_me_factor.clamp(0.01, 1.0)
    } else {
        1.0
    };

    d * health * aiming
}

/// Pick the enemy to shoot at, if any.
///
/// Only living, visible enemies are considered — an unseen enemy is a
/// navigation problem, not a shooting one.
pub fn select_target<'a>(world: &'a WorldView, params: &EngageParams) -> Option<&'a PlayerView> {
    if !world.me.alive {
        return None;
    }
    world
        .visible_enemies()
        .filter(|p| distance(world.me.origin, p.origin) <= params.max_engage_distance)
        .min_by(|a, b| {
            threat_score(world, a, params)
                .partial_cmp(&threat_score(world, b, params))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

/// Where the bot should be looking to hit `target`.
pub fn desired_angles(world: &WorldView, target: &PlayerView) -> Angles {
    aim_angles(world.me.origin, crate::math::eye_position(target.origin))
}

/// The decision `engage` makes each tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Engagement {
    pub aim: Angles,
    pub fire: bool,
    /// True when the bot should push toward the target rather than hold.
    pub advance: bool,
    pub distance: f32,
}

/// Decide how to fight `target` this tick.
///
/// `current` is where the bot is presently looking; firing is gated on the aim
/// already being close, so the bot cannot snap-and-shoot in one tick.
pub fn engage(
    world: &WorldView,
    target: &PlayerView,
    current: Angles,
    params: &EngageParams,
    rng: &mut Rng,
) -> Engagement {
    let aim = desired_angles(world, target);
    let dist = distance(world.me.origin, target.origin);
    let on_target = aim_error(current, aim) <= params.fire_cone_degrees;
    // Randomised trigger: `fireButtons` does not simply hold +attack.
    let fire = on_target && rng.chance(params.fire_chance);
    Engagement {
        aim,
        fire,
        advance: dist > params.close_quarters_distance,
        distance: dist,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{SelfState, Team};

    fn enemy(entity: u16, x: f32, health: f32) -> PlayerView {
        PlayerView {
            entity,
            origin: [x, 0.0, 0.0],
            health,
            team: Team::CounterTerrorist,
            alive: true,
            visible: true,
            angles: None,
        }
    }

    fn world_with(players: Vec<PlayerView>) -> WorldView {
        WorldView {
            me: SelfState { team: Team::Terrorist, ..Default::default() },
            players,
            ..Default::default()
        }
    }

    #[test]
    fn the_closest_enemy_is_chosen() {
        let w = world_with(vec![enemy(1, 800.0, 100.0), enemy(2, 200.0, 100.0)]);
        let t = select_target(&w, &EngageParams::default()).unwrap();
        assert_eq!(t.entity, 2);
    }

    #[test]
    fn a_wounded_enemy_wins_a_tie() {
        let w = world_with(vec![enemy(1, 300.0, 100.0), enemy(2, 300.0, 10.0)]);
        let t = select_target(&w, &EngageParams::default()).unwrap();
        assert_eq!(t.entity, 2, "the nearly-dead one should be preferred");
    }

    #[test]
    fn health_never_outweighs_a_much_closer_enemy() {
        let w = world_with(vec![enemy(1, 100.0, 100.0), enemy(2, 900.0, 1.0)]);
        let t = select_target(&w, &EngageParams::default()).unwrap();
        assert_eq!(t.entity, 1, "distance must dominate the score");
    }

    #[test]
    fn enemies_beyond_the_engage_range_are_ignored() {
        let w = world_with(vec![enemy(1, 5000.0, 100.0)]);
        assert!(select_target(&w, &EngageParams::default()).is_none());
    }

    #[test]
    fn invisible_and_dead_enemies_are_never_targeted() {
        let mut unseen = enemy(1, 100.0, 100.0);
        unseen.visible = false;
        let mut dead = enemy(2, 120.0, 100.0);
        dead.alive = false;
        let w = world_with(vec![unseen, dead]);
        assert!(select_target(&w, &EngageParams::default()).is_none());
    }

    #[test]
    fn a_dead_bot_does_not_pick_targets() {
        let mut w = world_with(vec![enemy(1, 100.0, 100.0)]);
        w.me.alive = false;
        assert!(select_target(&w, &EngageParams::default()).is_none());
    }

    #[test]
    fn firing_requires_the_aim_to_be_on_target_already() {
        let w = world_with(vec![enemy(1, 500.0, 100.0)]);
        let t = *select_target(&w, &EngageParams::default()).unwrap();
        let params = EngageParams { fire_chance: 1.0, ..Default::default() };
        let mut rng = Rng::new(1);

        // Looking the wrong way entirely.
        let e = engage(&w, &t, Angles { pitch: 0.0, yaw: 180.0 }, &params, &mut rng);
        assert!(!e.fire, "must not fire while facing away");

        // Looking straight at them.
        let on = desired_angles(&w, &t);
        let e = engage(&w, &t, on, &params, &mut rng);
        assert!(e.fire, "should fire when on target");
    }

    #[test]
    fn trigger_is_randomised_not_held() {
        let w = world_with(vec![enemy(1, 500.0, 100.0)]);
        let t = *select_target(&w, &EngageParams::default()).unwrap();
        let on = desired_angles(&w, &t);
        let params = EngageParams { fire_chance: 0.5, ..Default::default() };
        let mut rng = Rng::new(42);

        let shots = (0..400)
            .filter(|_| engage(&w, &t, on, &params, &mut rng).fire)
            .count();
        assert!(
            (120..280).contains(&shots),
            "expected roughly half the ticks to fire, got {shots}"
        );
    }

    #[test]
    fn an_enemy_already_aiming_at_us_is_dealt_with_first() {
        // Same range, but one of them has us in their sights.
        let mut aiming = enemy(1, 400.0, 100.0);
        aiming.angles = Some(Angles { pitch: 0.0, yaw: 180.0 }); // looking back at us
        let mut oblivious = enemy(2, 380.0, 100.0);
        oblivious.angles = Some(Angles { pitch: 0.0, yaw: 90.0 }); // looking away
        let w = world_with(vec![aiming, oblivious]);
        let t = select_target(&w, &EngageParams::default()).unwrap();
        assert_eq!(t.entity, 1, "the one pointing at us, even though slightly further");
    }

    #[test]
    fn aiming_at_us_still_cannot_beat_a_much_closer_enemy() {
        let mut far_aiming = enemy(1, 900.0, 100.0);
        far_aiming.angles = Some(Angles { pitch: 0.0, yaw: 180.0 });
        let close = enemy(2, 100.0, 100.0);
        let w = world_with(vec![far_aiming, close]);
        let t = select_target(&w, &EngageParams::default()).unwrap();
        assert_eq!(t.entity, 2, "distance must still dominate");
    }

    #[test]
    fn unknown_angles_do_not_promote_anyone() {
        // The default snapshot has no angles. Nobody should be treated as
        // threatening on the strength of a missing field.
        let w = world_with(vec![enemy(1, 400.0, 100.0), enemy(2, 380.0, 100.0)]);
        let params = EngageParams::default();
        let t = select_target(&w, &params).unwrap();
        assert_eq!(t.entity, 2, "falls back to plain distance");
        for p in &w.players {
            assert_eq!(threat_score(&w, p, &params), distance(w.me.origin, p.origin));
        }
    }

    #[test]
    fn the_health_term_is_inert_when_health_is_unobservable() {
        // A real server reports the same health for everyone. Prove the score
        // then reduces exactly to distance, so nothing hinges on the field.
        let params = EngageParams::default();
        let w = world_with(vec![enemy(1, 300.0, 100.0), enemy(2, 700.0, 100.0)]);
        for p in &w.players {
            let s = threat_score(&w, p, &params);
            assert!(
                (s - distance(w.me.origin, p.origin)).abs() < 1e-3,
                "score {s} should just be the distance"
            );
        }
    }

    #[test]
    fn recovered_constants_are_recorded_in_ascending_order() {
        let c = RECOVERED_ENGAGE_CONSTANTS;
        assert!(c.windows(2).all(|w| w[0] < w[1]));
        assert!(c.contains(&340.0), "the five-way threshold must be recorded");
    }
}
