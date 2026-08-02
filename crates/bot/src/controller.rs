//! The per-tick brain: fold a [`WorldView`] into a single [`Intent`].
//!
//! This is the seam the original keeps between `Think` (decide) and the
//! netcode (act). Everything here is engine-neutral: aim angles, movement
//! speeds, button intentions and console commands. The client layer maps an
//! `Intent` onto a `usercmd_t` and drains the commands onto the reliable
//! channel, so the whole decision path stays testable without a server.
//!
//! ## The ladder
//!
//! Strictly ordered, highest first. Each rung answers completely; there is no
//! blending, because blending two objectives is how a bot ends up walking into
//! a wall while half-planting.
//!
//! 1. **Dead** — hold. Nothing else is meaningful, and every machine resets.
//! 2. **Freeze period** — hold, and buy. `iuser3 & PLAYER_FREEZE_TIME_OVER` is
//!    set *during* the freeze (`dlls/client.cpp:5104`), so this is observable.
//! 3. **A visible enemy** — fight. Aim, gate the trigger on the weapon's real
//!    state, and either close or hold ground.
//! 4. **An objective already under way** — finish it. A plant three-quarters
//!    done is worth more than a fresh idea.
//! 5. **The objective** — walk to it, or escort a hostage.
//! 6. **Nothing** — hold position and drift, so the idle check stays happy.
//!
//! ## Two things this file gets right that are easy to get wrong
//!
//! **The internal aim is the bullet direction, not the wire value.** The
//! controller smooths [`Controller::view`] toward where the bullet should go,
//! and compensates for punchangle only at the last moment, when building the
//! `Intent`. Smoothing the compensated value instead would feed recoil back
//! into the aim loop and make the bot chase its own kick.
//!
//! **The bot does not fire the instant it sees someone.** A reaction delay is
//! held per target entity, so a new enemy has to be visible for
//! [`Difficulty::reaction_time`] before the trigger is available — and swapping
//! to a different target restarts it.

use crate::aim::{aim_error, compensate, predict_punch, turn_toward};
use crate::combat::{engage, select_target, EngageParams};
use crate::fire::FireControl;
use crate::idle::AntiIdle;
use crate::math::{aim_angles, distance2d, norm_angle, Angles, Vec3};

// Re-exported so `bot::controller::Intent` keeps resolving for callers that
// imported it from here before the type moved to its own module.
pub use crate::intent::{BotCommand, Intent};
use crate::objective::bomb::{DefuseMachine, PlantMachine};
use crate::objective::hostage::{HostageEscort, ESCORT_WALK_SPEED};
use crate::objective::{Objective, ObjectiveState};
use crate::rng::Rng;
use crate::task::Difficulty;
use crate::weapons::WeaponId;
use crate::world::WorldView;

/// Full running speed. CS 1.6's `cl_forwardspeed`/`cl_sidespeed` default is
/// 400, but the weapon-carry cap is 250; the bot moves at the cap it can
/// actually sustain. **Conventional, not recovered from the binary.**
pub const FORWARD_SPEED: f32 = 250.0;

/// How close (horizontally) counts as having reached a move target.
/// **Conventional**, chosen to be under one player width.
pub const ARRIVE_RADIUS: f32 = 24.0;

/// Speed at which the bot makes no footstep noise.
///
/// `PM_UpdateStepSound` returns without a sound at `speed <= 150.0`
/// (`pm_shared/pm_shared.cpp:395`). 130 leaves margin for the fact that the
/// speed compared there is the resulting *velocity*, not the requested move.
pub const WALK_SPEED: f32 = ESCORT_WALK_SPEED;

/// How often the per-target aim error is re-drawn, in seconds.
///
/// **Chosen.** Re-rolling every tick would be white noise, which averages to a
/// perfect aim over any burst and so makes the error free; re-rolling never
/// would be a fixed, learnable bias. A few hundred milliseconds reads as a hand
/// that is not quite steady.
pub const AIM_ERROR_REFRESH: f32 = 0.35;

/// Holds the bot's cross-tick state: where it is currently looking (aim is
/// smoothed, not teleported), how far along the objectives it is, and the
/// trigger latches.
#[derive(Debug, Clone)]
pub struct Controller {
    pub params: EngageParams,
    pub difficulty: Difficulty,
    pub rng: Rng,
    /// Current smoothed view angle, carried between ticks.
    ///
    /// This is **where the bullets should go**, not what gets sent — see the
    /// module docs.
    pub view: Angles,
    pub objective: ObjectiveState,
    pub plant: PlantMachine,
    pub defuse: DefuseMachine,
    pub escort: HostageEscort,
    pub fire: FireControl,
    pub idle: AntiIdle,
    /// The target being tracked, and for how long it has been visible.
    tracking: Option<(u16, f32)>,
    /// The current aim error, in degrees, and how long it has been held.
    aim_offset: (Angles, f32),
    /// The weapon we last saw ourselves holding, so the fire latches can be
    /// cleared on a switch.
    last_weapon: WeaponId,
    /// Which rung of the ladder produced the last [`Intent`].
    ///
    /// Purely diagnostic, and worth the field. The ladder is exclusive by
    /// design, so "the bot stood still" has as many explanations as there are
    /// rungs and no way to tell them apart from the outside -- every one of
    /// them can legitimately emit `forwardmove: 0.0`. Reading this off a live
    /// run replaces an afternoon of narrowing down which branch it was.
    pub rung: &'static str,
}

impl Controller {
    pub fn new(seed: u64, difficulty: Difficulty) -> Self {
        Self {
            params: EngageParams::default(),
            difficulty,
            rng: Rng::new(seed),
            view: Angles::default(),
            objective: ObjectiveState::default(),
            plant: PlantMachine::default(),
            defuse: DefuseMachine::default(),
            escort: HostageEscort::default(),
            fire: FireControl::new(difficulty.fire_params()),
            idle: AntiIdle::default(),
            tracking: None,
            aim_offset: (Angles::default(), f32::INFINITY),
            last_weapon: WeaponId::None,
            rung: "init",
        }
    }

    /// The aim error to apply this tick, re-drawn periodically.
    ///
    /// A fresh draw whenever the target changes, and every
    /// [`AIM_ERROR_REFRESH`] seconds otherwise. `fresh` forces a re-draw.
    ///
    /// The magnitude is [`Difficulty::aim_error_degrees`] used as a radius
    /// rather than a true standard deviation — a uniform disc, not a Gaussian.
    /// The distinction is not worth a normal-distribution sampler here: what
    /// matters behaviourally is the scale, and a disc has the useful property
    /// of a hard bound, so a difficulty can never produce a wild outlier.
    fn aim_error_offset(&mut self, fresh: bool, dt: f32) -> Angles {
        self.aim_offset.1 += dt;
        if fresh || self.aim_offset.1 >= AIM_ERROR_REFRESH {
            let radius = self.difficulty.aim_error_degrees();
            let angle = self.rng.range(0.0, std::f64::consts::TAU);
            // sqrt keeps the draw uniform over the disc rather than clustered
            // in the middle.
            let r = radius * self.rng.unit().sqrt();
            self.aim_offset = (
                Angles {
                    pitch: (r * angle.sin()) as f32,
                    yaw: (r * angle.cos()) as f32,
                },
                0.0,
            );
        }
        self.aim_offset.0
    }

    /// Turn the internal aim into the angles that actually go on the wire.
    ///
    /// Punchangle lands on the shot direction twice, and the value we hold is a
    /// round trip stale, so it is decayed forward first. See
    /// [`crate::aim::compensate`].
    fn wire_view(&self, world: &WorldView) -> Angles {
        let punch = predict_punch(
            world.me.punchangle,
            world.frametime,
            world.punch_prediction_frames(),
        );
        compensate(self.view, punch)
    }

    /// Reset everything that describes a life or a specific weapon.
    fn reset_for_death(&mut self) {
        self.objective = ObjectiveState::default();
        self.plant.reset();
        self.defuse.reset();
        self.escort.reset();
        self.fire.reset();
        self.tracking = None;
    }

    /// How long the current target has been visible, advancing the counter.
    ///
    /// Returns `true` once the reaction delay has elapsed. Switching targets
    /// restarts the clock, which is both realistic and the reason a bot cannot
    /// flick between two enemies faster than it could see either.
    fn reacted(&mut self, entity: u16, dt: f32) -> bool {
        let seen = match self.tracking {
            Some((e, t)) if e == entity => t + dt,
            _ => 0.0,
        };
        self.tracking = Some((entity, seen));
        seen >= self.difficulty.reaction_time()
    }

    /// Decide what to do this tick.
    ///
    /// `site` is the nearest bomb-site position (a `GOAL` node from the nav
    /// graph) used for plant/defuse; pass `None` when unknown. `dt` is the tick
    /// length in seconds.
    ///
    /// A pure function of `(WorldView, site, dt)` and the controller's own
    /// state — no clock, no network, no globals.
    pub fn think(&mut self, world: &WorldView, site: Option<Vec3>, dt: f32) -> Intent {
        self.idle.advance(dt);

        // A weapon switch invalidates the trigger latches: the burst counter
        // and the pistol release rule both describe one specific gun.
        let now_holding = world.me.weapon_or_unknown().id;
        if now_holding != self.last_weapon {
            self.fire.reset();
            self.last_weapon = now_holding;
        }

        // --- 1) Dead -------------------------------------------------------
        if !world.me.alive {
            self.reset_for_death();
            self.rung = "dead";
            return Intent::hold(self.wire_view(world));
        }

        let max_turn = self.difficulty.max_turn();

        // --- 2) Freeze period ---------------------------------------------
        // Movement is refused and weapons are locked; the only useful thing is
        // to shop. Note the flag's name is inverted — see `SelfState`.
        if world.me.freeze_period {
            self.fire.reset();
            self.tracking = None;
            let view = self.idle.apply(self.wire_view(world));
            let mut intent = Intent::hold(view);
            intent.commands = self.buy_plan(world);
            self.rung = "freeze";
            return intent;
        }

        // Keep the navigation-level objective in step regardless of what the
        // ladder below decides — it tracks arrival and mode, not buttons.
        self.objective.tick(world, site, dt);

        // --- 3) A visible enemy -------------------------------------------
        if let Some(target) = select_target(world, &self.params).copied() {
            let eng = engage(world, &target, self.view, &self.params, &mut self.rng);

            // A new target is a new mistake: re-draw the aim error rather than
            // carrying the last one across, which would otherwise let a bot
            // that had settled on one enemy snap perfectly onto the next.
            let switched = !matches!(self.tracking, Some((e, _)) if e == target.entity);
            let err = self.aim_error_offset(switched, dt);
            let intended = Angles {
                pitch: (eng.aim.pitch + err.pitch)
                    .clamp(-crate::aim::PITCH_LIMIT, crate::aim::PITCH_LIMIT),
                yaw: norm_angle(f64::from(eng.aim.yaw + err.yaw)) as f32,
            };
            self.view = turn_toward(self.view, intended, max_turn);

            // Three independent gates, all of which must pass. The cone is
            // measured against where the bot *thinks* it should be pointing —
            // the error is a mistake it is not aware of making.
            let reacted = self.reacted(target.entity, dt);
            let on_target = aim_error(self.view, intended) <= self.params.fire_cone_degrees;
            let want = reacted && on_target && eng.fire && world.me.can_shoot;

            let action = self.fire.decide(&world.me.weapon_or_unknown(), want);

            // A plant interrupted by a firefight is a plant that was released,
            // which the server has already cancelled. Record it honestly.
            if self.plant.is_arming() {
                self.plant.note_released();
            }

            self.rung = "combat";
            return Intent {
                view: self.wire_view(world),
                // Close only when we still need to; hold ground in a knife-fight
                // range so the aim can settle.
                forwardmove: if eng.advance { FORWARD_SPEED } else { 0.0 },
                attack: action.attack,
                reload: action.reload,
                move_target: Some(target.origin),
                ..Intent::default()
            };
        }
        self.tracking = None;

        // --- 4/5) Objectives ----------------------------------------------
        // Defuse first: a live bomb is a countdown and outranks everything.
        let defuse = self.defuse.tick(world, self.view, dt);
        if self.defuse.is_defusing() || defuse.use_action || defuse.move_to.is_some() {
            if let Some(look) = defuse.look_at {
                self.view = turn_toward(self.view, aim_angles(world.me.origin, look), max_turn);
            }
            self.rung = "defuse";
            return Intent {
                view: self.wire_view(world),
                forwardmove: if defuse.move_to.is_some() { FORWARD_SPEED } else { 0.0 },
                use_action: defuse.use_action,
                move_target: defuse.move_to,
                ..Intent::default()
            };
        }

        // Plant: the machine owns the button; this rung owns getting there.
        if world.bomb.carried_by_me && !world.bomb.planted {
            if let Some(target) = self.objective.target {
                let arrived = distance2d(world.me.origin, target) < ARRIVE_RADIUS
                    || self.objective.objective == Objective::Planting;
                self.view =
                    turn_toward(self.view, aim_angles(world.me.origin, target), max_turn);

                let out = self.plant.tick(world, dt);
                let mut intent = Intent {
                    view: self.wire_view(world),
                    // Standing still to plant is not optional: the server
                    // freezes the planter anyway, and drifting out of the zone
                    // cancels the whole thing.
                    forwardmove: if arrived { 0.0 } else { FORWARD_SPEED },
                    attack: out.attack,
                    move_target: Some(target),
                    ..Intent::default()
                };
                if let Some(w) = out.select {
                    intent.commands.push(BotCommand::Select(w));
                }
                self.rung = if arrived { "plant" } else { "plant-walk" };
                return intent;
            }
        }

        // Hostages.
        let escort = self.escort.tick(world, self.view, dt);
        if self.escort.is_busy() {
            if let Some(look) = escort.look_at {
                self.view = turn_toward(self.view, aim_angles(world.me.origin, look), max_turn);
            }
            let speed = if escort.walk { WALK_SPEED } else { FORWARD_SPEED };
            self.rung = "hostage";
            return Intent {
                view: self.wire_view(world),
                forwardmove: if escort.move_to.is_some() { speed } else { 0.0 },
                use_action: escort.use_action,
                walk: escort.walk,
                move_target: escort.move_to,
                ..Intent::default()
            };
        }

        // Whatever the navigation layer wants next.
        //
        // Falls back to the caller's `site` when no objective machine has
        // claimed one. That fallback is what makes the bot move at all in the
        // ordinary case: the bomb machine deliberately clears its target for a
        // terrorist who is not carrying the C4 (`objective.rs` -- only the
        // carrier plants), so without this a bot with a perfectly good route
        // to a bomb site stands still and the navigation layer looks broken
        // when it is working. Measured before the fix:
        //
        //     brain: alive true fwd 0 side 0 site Some([-1400,2320]) wp 5
        //
        // Going to the site is right whether or not there is an objective to
        // perform there -- it is where the round happens.
        if let Some(t) = self.objective.target.or(site) {
            self.view = turn_toward(self.view, aim_angles(world.me.origin, t), max_turn);
            let arrived = distance2d(world.me.origin, t) < ARRIVE_RADIUS;
            self.rung = if arrived { "arrived" } else { "goto" };
            return Intent {
                view: self.wire_view(world),
                forwardmove: if arrived { 0.0 } else { FORWARD_SPEED },
                move_target: Some(t),
                ..Intent::default()
            };
        }

        // --- 6) Nothing ----------------------------------------------------
        // Hold position, but keep drifting: `CheckActivityInGame` needs both
        // axes to have moved by 0.1 degrees between two samples five seconds
        // apart, and standing perfectly still gets the bot kicked.
        let action = self.fire.decide(&world.me.weapon_or_unknown(), false);
        self.rung = "idle";
        Intent {
            view: self.idle.apply(self.wire_view(world)),
            reload: action.reload,
            ..Intent::default()
        }
    }

    /// What to buy, given the money on hand.
    ///
    /// Deliberately minimal and deliberately ordered: armour first because it
    /// is the best value in the game, then a rifle, then a kit for CTs. The
    /// aliases are the bare buy words — `ak47`, not `weapon_ak47`, which would
    /// be a weapon switch (`dlls/client.cpp:3560`).
    pub fn buy_plan(&self, world: &WorldView) -> Vec<BotCommand> {
        use crate::weapons::Equipment;
        use crate::world::Team;

        let mut plan = Vec::new();
        let mut money = world.me.money;
        let mut spend = |cost: i32, cmd: BotCommand, plan: &mut Vec<BotCommand>| {
            if money >= cost {
                money -= cost;
                plan.push(cmd);
            }
        };

        // Kevlar + helmet, 1000.
        spend(1000, BotCommand::BuyEquipment(Equipment::VestHelm), &mut plan);

        // A rifle appropriate to the side.
        let rifle = match world.me.team {
            Team::Terrorist => (2500, WeaponId::Ak47),
            _ => (3100, WeaponId::M4a1),
        };
        spend(rifle.0, BotCommand::BuyWeapon(rifle.1), &mut plan);

        if world.me.team == Team::CounterTerrorist {
            spend(200, BotCommand::BuyEquipment(Equipment::Defuser), &mut plan);
        }
        spend(300, BotCommand::BuyEquipment(Equipment::HeGrenade), &mut plan);

        plan
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fire::WeaponState;
    use crate::weapons::Equipment;
    use crate::world::{BombState, HostageView, PlayerView, SelfState, Team};

    fn me_at(origin: Vec3, team: Team) -> SelfState {
        SelfState {
            origin,
            team,
            weapon: Some(WeaponState {
                id: WeaponId::Ak47,
                clip: 30,
                reserve: 90,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// A controller that reacts instantly, for tests about something else.
    fn instant(seed: u64) -> Controller {
        let mut c = Controller::new(seed, Difficulty::Unfair);
        c.params.fire_chance = 1.0;
        c
    }

    /// The ordinary case, and the one that was broken: a terrorist with no
    /// C4, no enemy in sight and somewhere to be must WALK there. The bomb
    /// machine clears its own target for a non-carrier, so the fallback to the
    /// caller's site is the only thing that moves this bot.
    #[test]
    fn a_bot_with_nothing_else_to_do_walks_to_the_site() {
        let mut c = Controller::new(7, Difficulty::Normal);
        let w = WorldView {
            me: me_at([0.0, 0.0, 0.0], Team::Terrorist),
            ..Default::default()
        };
        let site = [1000.0, 0.0, 0.0];
        let intent = c.think(&w, Some(site), 0.1);

        assert!(
            intent.forwardmove > 0.0,
            "should be walking toward the site, got {}",
            intent.forwardmove
        );
        assert_eq!(intent.move_target, Some(site));
    }

    /// ...and stops once it is there, rather than grinding into the wall.
    #[test]
    fn arriving_at_the_site_stops_the_walk() {
        let mut c = Controller::new(7, Difficulty::Normal);
        let w = WorldView {
            me: me_at([0.0, 0.0, 0.0], Team::Terrorist),
            ..Default::default()
        };
        let intent = c.think(&w, Some([4.0, 0.0, 0.0]), 0.1);
        assert_eq!(intent.forwardmove, 0.0, "already inside the arrive radius");
    }

    /// With no site and nothing to do it must still not freeze solid --
    /// `CheckActivityInGame` kicks a player whose view has not moved.
    #[test]
    fn with_no_site_at_all_the_bot_still_drifts_its_view() {
        let mut c = Controller::new(7, Difficulty::Normal);
        let w = WorldView {
            me: me_at([0.0, 0.0, 0.0], Team::Terrorist),
            ..Default::default()
        };
        let a = c.think(&w, None, 0.1);
        let b = c.think(&w, None, 0.1);
        assert_eq!(a.forwardmove, 0.0);
        assert!(
            (a.view.yaw - b.view.yaw).abs() > 0.0 || (a.view.pitch - b.view.pitch).abs() > 0.0,
            "the anti-idle drift must keep the view moving"
        );
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
        assert!(intent.commands.is_empty());
    }

    #[test]
    fn it_turns_toward_a_visible_enemy_and_eventually_fires() {
        let mut c = instant(7);
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
            me: SelfState {
                weapon: Some(WeaponState { id: WeaponId::C4, ..Default::default() }),
                ..me_at([0.0, 0.0, 0.0], Team::Terrorist)
            },
            bomb,
            ..Default::default()
        };
        let walking = c.think(&far, Some(site), 0.1);
        assert!(walking.forwardmove > 0.0, "walks toward the site");
        assert_eq!(walking.move_target, Some(site));
        assert!(!walking.attack);

        let onsite = WorldView {
            me: SelfState {
                in_bomb_zone: true,
                weapon: Some(WeaponState { id: WeaponId::C4, ..Default::default() }),
                ..me_at([1000.0, 10.0, 0.0], Team::Terrorist)
            },
            bomb,
            ..Default::default()
        };
        let planting = c.think(&onsite, Some(site), 0.1);
        assert!(planting.attack, "holds +attack to plant");
        assert_eq!(planting.forwardmove, 0.0, "stands still to plant");
    }

    #[test]
    fn the_carrier_switches_to_the_c4_before_pressing_anything() {
        let mut c = Controller::new(5, Difficulty::Normal);
        let site: Vec3 = [1000.0, 0.0, 0.0];
        let w = WorldView {
            me: SelfState {
                in_bomb_zone: true,
                ..me_at([1000.0, 10.0, 0.0], Team::Terrorist) // holding an AK
            },
            bomb: BombState { carried_by_me: true, ..Default::default() },
            ..Default::default()
        };
        let intent = c.think(&w, Some(site), 0.1);
        assert!(!intent.attack, "attacking with a rifle out just shoots the floor");
        assert_eq!(intent.console_lines(), vec!["weapon_c4".to_string()]);
    }

    #[test]
    fn a_plant_outside_the_bomb_zone_is_never_started() {
        let mut c = Controller::new(5, Difficulty::Normal);
        let site: Vec3 = [1000.0, 0.0, 0.0];
        let w = WorldView {
            me: SelfState {
                in_bomb_zone: false,
                weapon: Some(WeaponState { id: WeaponId::C4, ..Default::default() }),
                ..me_at([1000.0, 10.0, 0.0], Team::Terrorist)
            },
            bomb: BombState { carried_by_me: true, ..Default::default() },
            ..Default::default()
        };
        for _ in 0..40 {
            assert!(!c.think(&w, Some(site), 0.1).attack, "iuser3 said we are not in a zone");
        }
    }

    #[test]
    fn a_ct_defuses_with_use_not_attack() {
        let mut c = Controller::new(9, Difficulty::Unfair);
        let site: Vec3 = [1000.0, 0.0, 0.0];
        let w = WorldView {
            me: SelfState {
                has_defuse_kit: true,
                ..me_at([1000.0, 10.0, 0.0], Team::CounterTerrorist)
            },
            bomb: BombState {
                planted: true,
                origin: Some(site),
                ..Default::default()
            },
            ..Default::default()
        };
        // The first tick may still be turning to face the bomb; give the aim a
        // moment to come inside VIEW_FIELD_NARROW.
        let mut intent = c.think(&w, None, 0.1);
        for _ in 0..30 {
            if intent.use_action {
                break;
            }
            intent = c.think(&w, None, 0.1);
        }
        assert!(intent.use_action, "defuse holds +use");
        assert!(!intent.attack);
        assert_eq!(intent.forwardmove, 0.0);
    }

    #[test]
    fn the_reaction_delay_actually_delays() {
        // An easy bot handed a perfectly aimed shot still may not take it.
        let mut c = Controller::new(11, Difficulty::Easy);
        c.params.fire_chance = 1.0;
        let w = WorldView {
            me: me_at([0.0, 0.0, 0.0], Team::Terrorist),
            players: vec![PlayerView {
                entity: 1,
                origin: [400.0, 0.0, 0.0],
                team: Team::CounterTerrorist,
                visible: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        // Start already pointing at them so aim convergence is not the gate.
        c.view = aim_angles(w.me.origin, crate::math::eye_position([400.0, 0.0, 0.0]));

        let dt = 0.05;
        let mut t = 0.0f32;
        let mut first_shot = None;
        for _ in 0..100 {
            if c.think(&w, None, dt).attack && first_shot.is_none() {
                first_shot = Some(t);
            }
            t += dt;
        }
        let shot_at = first_shot.expect("it should fire eventually");
        assert!(
            shot_at >= Difficulty::Easy.reaction_time() - dt,
            "fired after {shot_at}s, faster than the {}s reaction time",
            Difficulty::Easy.reaction_time()
        );
    }

    #[test]
    fn switching_target_restarts_the_reaction_clock() {
        let mut c = Controller::new(13, Difficulty::Normal);
        c.params.fire_chance = 1.0;
        let mut w = WorldView {
            me: me_at([0.0, 0.0, 0.0], Team::Terrorist),
            players: vec![PlayerView {
                entity: 1,
                origin: [300.0, 0.0, 0.0],
                team: Team::CounterTerrorist,
                visible: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        c.view = aim_angles(w.me.origin, crate::math::eye_position([300.0, 0.0, 0.0]));
        // Burn through the reaction time on target 1.
        for _ in 0..40 {
            c.think(&w, None, 0.05);
        }
        // Swap in a different entity at the same place.
        w.players[0].entity = 2;
        c.fire.reset();
        let immediate = c.think(&w, None, 0.05);
        assert!(!immediate.attack, "a new enemy must be reacted to, not inherited");
    }

    #[test]
    fn a_pistol_is_never_held_down_even_in_a_long_fight() {
        let mut c = instant(17);
        let w = WorldView {
            me: SelfState {
                weapon: Some(WeaponState {
                    id: WeaponId::Deagle,
                    clip: 7,
                    reserve: 35,
                    ..Default::default()
                }),
                ..me_at([0.0, 0.0, 0.0], Team::Terrorist)
            },
            players: vec![PlayerView {
                entity: 1,
                origin: [300.0, 0.0, 0.0],
                team: Team::CounterTerrorist,
                visible: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        c.view = aim_angles(w.me.origin, crate::math::eye_position([300.0, 0.0, 0.0]));

        let mut prev = false;
        for tick in 0..200 {
            let a = c.think(&w, None, 0.05).attack;
            assert!(!(a && prev), "held the deagle trigger at tick {tick}");
            prev = a;
        }
    }

    #[test]
    fn nothing_is_fired_while_the_weapon_is_not_ready() {
        let mut c = instant(19);
        let w = WorldView {
            me: SelfState {
                weapon: Some(WeaponState {
                    id: WeaponId::Ak47,
                    clip: 30,
                    reserve: 90,
                    next_primary_attack: 0.5, // still counting down
                    ..Default::default()
                }),
                ..me_at([0.0, 0.0, 0.0], Team::Terrorist)
            },
            players: vec![PlayerView {
                entity: 1,
                origin: [300.0, 0.0, 0.0],
                team: Team::CounterTerrorist,
                visible: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        c.view = aim_angles(w.me.origin, crate::math::eye_position([300.0, 0.0, 0.0]));
        for _ in 0..60 {
            assert!(!c.think(&w, None, 0.05).attack, "m_flNextPrimaryAttack > 0");
        }
    }

    #[test]
    fn nothing_is_fired_at_an_enemy_that_cannot_be_seen() {
        let mut c = instant(23);
        let w = WorldView {
            me: me_at([0.0, 0.0, 0.0], Team::Terrorist),
            players: vec![PlayerView {
                entity: 1,
                origin: [200.0, 0.0, 0.0],
                team: Team::CounterTerrorist,
                visible: false, // behind a wall
                ..Default::default()
            }],
            ..Default::default()
        };
        c.view = aim_angles(w.me.origin, crate::math::eye_position([200.0, 0.0, 0.0]));
        for _ in 0..60 {
            let i = c.think(&w, None, 0.05);
            assert!(!i.attack, "shooting at an unseen enemy is shooting at a wall");
        }
    }

    #[test]
    fn a_bot_forbidden_to_shoot_does_not_shoot() {
        // iuser3 & PLAYER_CAN_SHOOT is cleared while defusing, among others.
        let mut c = instant(29);
        let w = WorldView {
            me: SelfState { can_shoot: false, ..me_at([0.0, 0.0, 0.0], Team::Terrorist) },
            players: vec![PlayerView {
                entity: 1,
                origin: [300.0, 0.0, 0.0],
                team: Team::CounterTerrorist,
                visible: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        c.view = aim_angles(w.me.origin, crate::math::eye_position([300.0, 0.0, 0.0]));
        for _ in 0..60 {
            assert!(!c.think(&w, None, 0.05).attack);
        }
    }

    #[test]
    fn the_sent_angles_are_punch_compensated_but_the_internal_aim_is_not() {
        let mut c = instant(31);
        let w = WorldView {
            me: SelfState {
                punchangle: Angles { pitch: -3.0, yaw: 0.0 },
                ..me_at([0.0, 0.0, 0.0], Team::Terrorist)
            },
            players: vec![PlayerView {
                entity: 1,
                origin: [400.0, 0.0, 0.0],
                team: Team::CounterTerrorist,
                visible: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        let intent = c.think(&w, None, 0.05);
        // Sent pitch is the aim plus 2 * 3 degrees of down-correction.
        assert!(
            (intent.view.pitch - (c.view.pitch + 6.0)).abs() < 1e-3,
            "sent {} vs internal {}",
            intent.view.pitch,
            c.view.pitch
        );
        // And the internal aim did not absorb the recoil.
        assert!(c.view.pitch.abs() < 1.0, "internal aim drifted to {}", c.view.pitch);
    }

    #[test]
    fn an_idle_bot_keeps_drifting_so_it_is_not_kicked() {
        // The full predicate is tested in `idle`; this checks the controller
        // actually applies it on the do-nothing rung.
        let mut c = Controller::new(37, Difficulty::Normal);
        let w = WorldView {
            me: me_at([0.0, 0.0, 0.0], Team::Terrorist),
            ..Default::default()
        };
        let first = c.think(&w, None, 0.05);
        for _ in 0..100 {
            c.think(&w, None, 0.05);
        }
        let later = c.think(&w, None, 0.05);
        assert!(
            (first.view.yaw - later.view.yaw).abs() >= 0.1,
            "yaw did not drift: {} -> {}",
            first.view.yaw,
            later.view.yaw
        );
        assert!(
            (first.view.pitch - later.view.pitch).abs() >= 0.1,
            "pitch did not drift: {} -> {}",
            first.view.pitch,
            later.view.pitch
        );
    }

    #[test]
    fn the_freeze_period_buys_and_stands_still() {
        let mut c = Controller::new(41, Difficulty::Normal);
        let w = WorldView {
            me: SelfState {
                freeze_period: true,
                money: 16000,
                ..me_at([0.0, 0.0, 0.0], Team::Terrorist)
            },
            ..Default::default()
        };
        let intent = c.think(&w, None, 0.05);
        assert_eq!(intent.forwardmove, 0.0);
        assert!(!intent.attack);
        let lines = intent.console_lines();
        assert!(lines.contains(&"vesthelm".to_string()), "{lines:?}");
        assert!(lines.contains(&"ak47".to_string()), "a T buys an AK: {lines:?}");
        assert!(!lines.iter().any(|l| l.starts_with("weapon_")), "buys are not switches");
    }

    #[test]
    fn a_poor_bot_buys_only_what_it_can_afford() {
        // The invariant that matters is that the plan never overspends and
        // never lists something out of reach. It is greedy in priority order,
        // so a bot that cannot afford armour still picks up the cheap things.
        let c = Controller::new(43, Difficulty::Normal);

        let w = WorldView {
            me: SelfState { money: 900, ..me_at([0.0; 3], Team::CounterTerrorist) },
            ..Default::default()
        };
        let plan = c.buy_plan(&w);
        assert!(
            !plan.contains(&BotCommand::BuyEquipment(Equipment::VestHelm)),
            "1000 of armour is out of reach at 900: {plan:?}"
        );
        assert!(
            !plan.contains(&BotCommand::BuyWeapon(WeaponId::M4a1)),
            "and so is a rifle: {plan:?}"
        );
        assert_eq!(
            plan,
            vec![
                BotCommand::BuyEquipment(Equipment::Defuser),
                BotCommand::BuyEquipment(Equipment::HeGrenade),
            ]
        );

        let w = WorldView {
            me: SelfState { money: 1250, ..me_at([0.0; 3], Team::CounterTerrorist) },
            ..Default::default()
        };
        let plan = c.buy_plan(&w);
        assert_eq!(
            plan,
            vec![
                BotCommand::BuyEquipment(Equipment::VestHelm),
                BotCommand::BuyEquipment(Equipment::Defuser),
            ],
            "armour first, then the kit — no rifle, and no nade left at 50"
        );
    }

    #[test]
    fn a_buy_plan_never_spends_money_the_bot_does_not_have() {
        // Prices as of the CS 1.6 defaults; the plan is greedy in the order
        // `buy_plan` writes, so this is exact, not an approximation.
        fn cost(cmd: &BotCommand) -> i32 {
            match cmd {
                BotCommand::BuyEquipment(Equipment::VestHelm) => 1000,
                BotCommand::BuyEquipment(Equipment::Defuser) => 200,
                BotCommand::BuyEquipment(Equipment::HeGrenade) => 300,
                BotCommand::BuyWeapon(WeaponId::Ak47) => 2500,
                BotCommand::BuyWeapon(WeaponId::M4a1) => 3100,
                other => panic!("unpriced item in the plan: {other:?}"),
            }
        }
        let c = Controller::new(71, Difficulty::Normal);
        for team in [Team::Terrorist, Team::CounterTerrorist] {
            for money in (0..17_000).step_by(137) {
                let w = WorldView {
                    me: SelfState { money, ..me_at([0.0; 3], team) },
                    ..Default::default()
                };
                let plan = c.buy_plan(&w);
                let total: i32 = plan.iter().map(cost).sum();
                assert!(total <= money, "{team:?} with {money} planned {total}: {plan:?}");
                // Every entry must actually be renderable as a console command.
                for cmd in &plan {
                    assert!(cmd.to_console().is_some(), "unrenderable {cmd:?}");
                }
            }
        }
    }

    #[test]
    fn a_ct_escorts_a_hostage_and_slows_when_it_trails() {
        let mut c = Controller::new(47, Difficulty::Normal);
        let w = WorldView {
            me: me_at([0.0, 0.0, 0.0], Team::CounterTerrorist),
            hostages: vec![HostageView {
                entity: 1,
                origin: [170.0, 0.0, 0.0], // past the leash
                following_me: true,
                ..Default::default()
            }],
            rescue_zones: vec![[-2000.0, 0.0, 0.0]],
            ..Default::default()
        };
        let intent = c.think(&w, None, 0.05);
        assert!(intent.walk, "should slow for a trailing hostage");
        assert!(intent.forwardmove > 0.0 && intent.forwardmove <= WALK_SPEED);
        assert!(WALK_SPEED < 150.0, "and stay under the footstep threshold");
        assert_eq!(intent.move_target, Some([-2000.0, 0.0, 0.0]));
    }

    #[test]
    fn a_hostage_is_never_used_on_two_consecutive_ticks() {
        let mut c = Controller::new(53, Difficulty::Normal);
        let w = WorldView {
            me: me_at([140.0, 0.0, 0.0], Team::CounterTerrorist),
            hostages: vec![HostageView { entity: 1, origin: [100.0, 0.0, 0.0], ..Default::default() }],
            rescue_zones: vec![[-2000.0, 0.0, 0.0]],
            ..Default::default()
        };
        let mut prev = false;
        let mut edges = 0;
        for tick in 0..200 {
            let u = c.think(&w, None, 0.05).use_action;
            assert!(!(u && prev), "held +use on a FCAP_ONOFF_USE hostage at tick {tick}");
            if u && !prev {
                edges += 1;
            }
            prev = u;
        }
        // 200 ticks * 50 ms = 10 s, and the hostage locks for 1 s per toggle.
        assert!(edges >= 1, "it should try at least once");
        assert!(edges <= 10, "{edges} edges in 10 s exceeds the toggle rate");
    }

    #[test]
    fn a_fight_takes_priority_over_a_hostage() {
        let mut c = instant(59);
        let w = WorldView {
            me: me_at([140.0, 0.0, 0.0], Team::CounterTerrorist),
            players: vec![PlayerView {
                entity: 9,
                origin: [140.0, 300.0, 0.0],
                team: Team::Terrorist,
                visible: true,
                ..Default::default()
            }],
            hostages: vec![HostageView { entity: 1, origin: [100.0, 0.0, 0.0], ..Default::default() }],
            rescue_zones: vec![[-2000.0, 0.0, 0.0]],
            ..Default::default()
        };
        let intent = c.think(&w, None, 0.05);
        assert!(!intent.use_action, "do not fumble with a hostage mid-firefight");
        assert_eq!(intent.move_target, Some([140.0, 300.0, 0.0]));
    }

    #[test]
    fn switching_weapon_clears_the_trigger_latches() {
        let mut c = instant(61);
        let mut w = WorldView {
            me: SelfState {
                weapon: Some(WeaponState {
                    id: WeaponId::Usp,
                    clip: 12,
                    reserve: 100,
                    ..Default::default()
                }),
                ..me_at([0.0, 0.0, 0.0], Team::Terrorist)
            },
            players: vec![PlayerView {
                entity: 1,
                origin: [300.0, 0.0, 0.0],
                team: Team::CounterTerrorist,
                visible: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        c.view = aim_angles(w.me.origin, crate::math::eye_position([300.0, 0.0, 0.0]));
        // Fire the pistol so the "must release" latch is set.
        let mut fired = false;
        for _ in 0..10 {
            if c.think(&w, None, 0.05).attack {
                fired = true;
                break;
            }
        }
        assert!(fired);
        assert!(c.fire.was_holding());

        // Swap to a rifle: the pistol's latch must not eat the first shot.
        w.me.weapon = Some(WeaponState {
            id: WeaponId::Ak47,
            clip: 30,
            reserve: 90,
            ..Default::default()
        });
        assert!(c.think(&w, None, 0.05).attack, "the rifle should fire immediately");
    }

    #[test]
    fn an_empty_gun_reloads_rather_than_clicking() {
        let mut c = Controller::new(67, Difficulty::Normal);
        let w = WorldView {
            me: SelfState {
                weapon: Some(WeaponState {
                    id: WeaponId::Ak47,
                    clip: 0,
                    reserve: 90,
                    ..Default::default()
                }),
                ..me_at([0.0, 0.0, 0.0], Team::Terrorist)
            },
            players: vec![PlayerView {
                entity: 1,
                origin: [300.0, 0.0, 0.0],
                team: Team::CounterTerrorist,
                visible: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        let intent = c.think(&w, None, 0.05);
        assert!(!intent.attack);
        assert!(intent.reload);
    }

    #[test]
    fn a_worse_bot_aims_worse() {
        // Same geometry, same seed, different difficulty: the settled aim must
        // sit further from the truth for the weaker bot.
        fn settled_error(d: Difficulty) -> f64 {
            let w = WorldView {
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
            let truth = aim_angles(w.me.origin, crate::math::eye_position([600.0, 0.0, 0.0]));
            let mut c = Controller::new(4242, d);
            let mut total = 0.0;
            let mut n = 0.0;
            for tick in 0..600 {
                c.think(&w, None, 0.05);
                if tick > 60 {
                    // Only sample once the turn has had time to converge.
                    total += aim_error(c.view, truth);
                    n += 1.0;
                }
            }
            total / n
        }

        let easy = settled_error(Difficulty::Easy);
        let normal = settled_error(Difficulty::Normal);
        let unfair = settled_error(Difficulty::Unfair);
        assert!(easy > normal, "easy {easy} should be worse than normal {normal}");
        assert!(normal > unfair, "normal {normal} should be worse than unfair {unfair}");
        assert!(unfair < 0.5, "unfair should be essentially perfect, was {unfair}");
        assert!(
            easy <= Difficulty::Easy.aim_error_degrees() * 1.5,
            "easy error {easy} exceeds its own bound"
        );
    }

    #[test]
    fn the_aim_error_is_bounded_by_the_difficulty() {
        // A uniform disc, so no draw can ever exceed the stated radius.
        for d in [Difficulty::Easy, Difficulty::Normal, Difficulty::Hard, Difficulty::Unfair] {
            let mut c = Controller::new(99, d);
            for _ in 0..5000 {
                let e = c.aim_error_offset(true, 0.05);
                let r = (f64::from(e.pitch).powi(2) + f64::from(e.yaw).powi(2)).sqrt();
                assert!(r <= d.aim_error_degrees() + 1e-6, "{d:?} drew {r}");
            }
        }
    }

    #[test]
    fn the_aim_error_is_held_rather_than_re_rolled_every_tick() {
        // White noise would average out to a perfect aim over a burst.
        let mut c = Controller::new(7, Difficulty::Easy);
        let first = c.aim_error_offset(true, 0.0);
        for _ in 0..3 {
            assert_eq!(c.aim_error_offset(false, 0.05), first, "should be held");
        }
        // ...but not forever.
        for _ in 0..20 {
            c.aim_error_offset(false, 0.05);
        }
        assert_ne!(c.aim_error_offset(false, 0.05), first, "should have been re-drawn");
    }

    #[test]
    fn think_is_deterministic_for_a_given_seed() {
        // A pure function of (world, site, dt) and its own state — no clock.
        let w = WorldView {
            me: me_at([0.0, 0.0, 0.0], Team::Terrorist),
            players: vec![PlayerView {
                entity: 1,
                origin: [0.0, 400.0, 0.0],
                team: Team::CounterTerrorist,
                visible: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut a = Controller::new(101, Difficulty::Normal);
        let mut b = Controller::new(101, Difficulty::Normal);
        for tick in 0..50 {
            assert_eq!(a.think(&w, None, 0.05), b.think(&w, None, 0.05), "tick {tick}");
        }
    }
}
