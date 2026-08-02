//! The per-tick brain: fold a [`WorldView`] into a single [`Intent`].
//!
//! This is the seam the original keeps between `Think` (decide) and the
//! netcode (act). Everything here is engine-neutral: aim angles, movement
//! speeds and a few button intentions. The client layer maps an `Intent` onto
//! a `usercmd_t` and puts it on the wire, so the whole decision path stays
//! testable without a server.
//!
//! It composes the already-verified pieces — [`select_target`]/[`engage`] for
//! fighting, [`ObjectiveState`] for the bomb, [`turn_toward`] for aim
//! smoothing — rather than introducing new behaviour. The two conventional
//! constants it adds ([`FORWARD_SPEED`], [`ARRIVE_RADIUS`]) are the standard
//! CS 1.6 values, not recovered from the binary, and are marked as such.

use crate::aim::turn_toward;
use crate::combat::{engage, select_target, EngageParams};
use crate::math::{aim_angles, distance2d, Angles, Vec3};
use crate::objective::{Objective, ObjectiveState};
use crate::rng::Rng;
use crate::task::Difficulty;
use crate::world::WorldView;

/// Full running speed. CS 1.6's `cl_forwardspeed`/`cl_sidespeed` default is
/// 400, but the weapon-carry cap is 250; the bot moves at the cap it can
/// actually sustain. **Conventional, not recovered from the binary.**
pub const FORWARD_SPEED: f32 = 250.0;

/// How close (horizontally) counts as having reached a move target.
/// **Conventional**, chosen to be under one player width.
pub const ARRIVE_RADIUS: f32 = 24.0;

/// What the bot wants to do this tick, in engine-neutral terms.
///
/// The client turns this into a `usercmd_t`: `view` → `viewangles`,
/// `forwardmove`/`sidemove` straight across, and the button intentions into
/// the `buttons` bitmask.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Intent {
    pub view: Angles,
    pub forwardmove: f32,
    pub sidemove: f32,
    pub attack: bool,
    pub jump: bool,
    pub duck: bool,
    /// Hold `+use` — defusing, and picking things up.
    pub use_action: bool,
    /// Where the bot is trying to move, for tracing and the nav layer. `None`
    /// while holding position.
    pub move_target: Option<Vec3>,
}

impl Default for Intent {
    fn default() -> Self {
        Self {
            view: Angles::default(),
            forwardmove: 0.0,
            sidemove: 0.0,
            attack: false,
            jump: false,
            duck: false,
            use_action: false,
            move_target: None,
        }
    }
}

/// Holds the bot's cross-tick state: where it is currently looking (aim is
/// smoothed, not teleported) and how far along the bomb objective it is.
#[derive(Debug, Clone)]
pub struct Controller {
    pub params: EngageParams,
    pub difficulty: Difficulty,
    pub rng: Rng,
    /// Current smoothed view angle, carried between ticks.
    pub view: Angles,
    pub objective: ObjectiveState,
}

impl Controller {
    pub fn new(seed: u64, difficulty: Difficulty) -> Self {
        Self {
            params: EngageParams::default(),
            difficulty,
            rng: Rng::new(seed),
            view: Angles::default(),
            objective: ObjectiveState::default(),
        }
    }

    /// Decide what to do this tick.
    ///
    /// `site` is the nearest bomb-site position (a `GOAL` node from the nav
    /// graph) used for plant/defuse; pass `None` when unknown. `dt` is the tick
    /// length in seconds.
    ///
    /// Priority, highest first: **fight** a visible enemy, **finish** a plant
    /// or defuse already in progress, **walk** to the objective, otherwise
    /// hold. Aim is always smoothed through [`turn_toward`] at the difficulty's
    /// turn cap, so the bot never snaps.
    pub fn think(&mut self, world: &WorldView, site: Option<Vec3>, dt: f32) -> Intent {
        if !world.me.alive {
            // Nothing to do while dead; keep the view where it was.
            self.objective = ObjectiveState::default();
            return Intent { view: self.view, ..Intent::default() };
        }

        let max_turn = self.difficulty.max_turn();

        // Advance the bomb objective regardless — it tracks arrival/timers.
        self.objective.tick(world, site, dt);

        // 1) A visible enemy is the whole game: aim and (maybe) fire.
        if let Some(target) = select_target(world, &self.params).copied() {
            let eng = engage(world, &target, self.view, &self.params, &mut self.rng);
            self.view = turn_toward(self.view, eng.aim, max_turn);
            return Intent {
                view: self.view,
                // Close only when we still need to; hold ground in a knife-fight
                // range so the aim can settle.
                forwardmove: if eng.advance { FORWARD_SPEED } else { 0.0 },
                attack: eng.fire,
                move_target: Some(target.origin),
                ..Intent::default()
            };
        }

        // 2) Standing on the site mid-action: hold still and hold the key.
        if self.objective.is_acting() {
            if let Some(t) = self.objective.target {
                let want = aim_angles(world.me.origin, t);
                self.view = turn_toward(self.view, want, max_turn);
            }
            let planting = matches!(self.objective.objective, Objective::Planting);
            return Intent {
                view: self.view,
                attack: planting,
                use_action: !planting, // defusing is +use
                move_target: self.objective.target,
                ..Intent::default()
            };
        }

        // 3) Heading to the objective: walk toward it.
        if let Some(t) = self.objective.target {
            let want = aim_angles(world.me.origin, t);
            self.view = turn_toward(self.view, want, max_turn);
            let arrived = distance2d(world.me.origin, t) < ARRIVE_RADIUS;
            return Intent {
                view: self.view,
                forwardmove: if arrived { 0.0 } else { FORWARD_SPEED },
                move_target: Some(t),
                ..Intent::default()
            };
        }

        // 4) Nothing pressing. Hold position; the nav layer will supply roam
        //    waypoints once wired.
        Intent { view: self.view, ..Intent::default() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{BombState, PlayerView, SelfState, Team};

    fn me_at(origin: Vec3, team: Team) -> SelfState {
        SelfState { origin, team, ..Default::default() }
    }

    #[test]
    fn a_dead_bot_holds_and_does_nothing() {
        let mut c = Controller::new(1, Difficulty::Normal);
        let w = WorldView {
            me: SelfState { alive: false, ..me_at([0.0; 3], Team::Terrorist) },
            ..Default::default()
        };
        let intent = c.think(&w, None, 0.1);
        assert_eq!(intent.forwardmove, 0.0);
        assert!(!intent.attack);
        assert_eq!(intent.move_target, None);
    }

    #[test]
    fn it_turns_toward_a_visible_enemy_and_eventually_fires() {
        let mut c = Controller::new(7, Difficulty::Unfair);
        c.params.fire_chance = 1.0;
        let w = WorldView {
            me: me_at([0.0, 0.0, 0.0], Team::Terrorist),
            players: vec![PlayerView {
                entity: 1,
                origin: [0.0, 400.0, 0.0], // due +y, i.e. yaw 90
                team: Team::CounterTerrorist,
                visible: true,
                ..Default::default()
            }],
            ..Default::default()
        };

        // First tick: turning has begun but we are not on target yet.
        let first = c.think(&w, None, 0.1);
        assert!(first.move_target.is_some());
        assert!(!first.attack, "must not snap-fire on the first tick");

        // After enough ticks the aim settles and the trigger comes out.
        let mut fired = false;
        for _ in 0..50 {
            if c.think(&w, None, 0.1).attack {
                fired = true;
                break;
            }
        }
        assert!(fired, "should fire once the aim has converged");
        assert!((c.view.yaw - 90.0).abs() < 5.0, "aim {} ~ 90", c.view.yaw);
    }

    #[test]
    fn a_close_enemy_is_not_chased_but_a_far_one_is() {
        let mut c = Controller::new(3, Difficulty::Normal);
        let far = WorldView {
            me: me_at([0.0, 0.0, 0.0], Team::Terrorist),
            players: vec![PlayerView {
                entity: 1,
                origin: [600.0, 0.0, 0.0],
                team: Team::CounterTerrorist,
                visible: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(c.think(&far, None, 0.1).forwardmove > 0.0, "closes on a far enemy");

        let mut c = Controller::new(3, Difficulty::Normal);
        let close = WorldView {
            me: me_at([0.0, 0.0, 0.0], Team::Terrorist),
            players: vec![PlayerView {
                entity: 1,
                origin: [80.0, 0.0, 0.0], // inside close_quarters_distance
                team: Team::CounterTerrorist,
                visible: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(c.think(&close, None, 0.1).forwardmove, 0.0, "holds at knife range");
    }

    #[test]
    fn the_carrier_walks_to_the_site_then_plants() {
        let mut c = Controller::new(5, Difficulty::Normal);
        let site: Vec3 = [1000.0, 0.0, 0.0];
        let bomb = BombState { carried_by_me: true, ..Default::default() };

        let far = WorldView {
            me: me_at([0.0, 0.0, 0.0], Team::Terrorist),
            bomb,
            ..Default::default()
        };
        let walking = c.think(&far, Some(site), 0.1);
        assert!(walking.forwardmove > 0.0, "walks toward the site");
        assert_eq!(walking.move_target, Some(site));
        assert!(!walking.attack);

        let onsite = WorldView {
            me: me_at([1000.0, 10.0, 0.0], Team::Terrorist),
            bomb,
            ..Default::default()
        };
        let planting = c.think(&onsite, Some(site), 0.1);
        assert!(planting.attack, "holds +attack to plant");
        assert_eq!(planting.forwardmove, 0.0, "stands still to plant");
    }

    #[test]
    fn a_ct_defuses_with_use_not_attack() {
        let mut c = Controller::new(9, Difficulty::Normal);
        let site: Vec3 = [1000.0, 0.0, 0.0];
        let w = WorldView {
            me: SelfState {
                has_defuse_kit: true,
                ..me_at([1000.0, 10.0, 0.0], Team::CounterTerrorist)
            },
            bomb: BombState { planted: true, origin: Some(site), carried_by_me: false },
            ..Default::default()
        };
        let intent = c.think(&w, None, 0.1);
        assert!(intent.use_action, "defuse holds +use");
        assert!(!intent.attack);
        assert_eq!(intent.forwardmove, 0.0);
    }
}
