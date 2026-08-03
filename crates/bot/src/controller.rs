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
use crate::math::{aim_angles, distance2d, move_axes, norm_angle, Angles, Vec3};

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

/// How close an enemy has to be before the bomb carrier will stop for it.
///
/// **The carrier's job is the plant, not the duel.** A bot that trades with
/// every counter-terrorist it sees dies in the open with the bomb, and the
/// round is over before anyone reaches a site. Measured: ten bots, five a side,
/// ten minutes, **zero plants** -- the carrier was killed first in every single
/// round, while an unopposed one plants in about thirty-five seconds.
///
/// Inside this radius running is not an option and it fights like anyone else.
/// Beyond it, it keeps going -- which is exactly what a human carrier does, and
/// it is also why teammates exist to trade on its behalf.
///
/// **Chosen.** Roughly the distance a player crosses in a second and a half at
/// full speed, so an enemy further away than this is one the carrier can
/// realistically disengage from.
pub const CARRIER_HOLDS_FIRE_BEYOND: f32 = 400.0;

/// Speed below which a rifle is accurate.
///
/// Every rifle picks its spread with `if (velocity.Length() > 140)` and the
/// moving branch is two to three times worse than the standing one
/// (`wpn_ak47.cpp:75-86`, `wpn_m4a1.cpp:103-128`, and the same shape in
/// aug/sg552/galil/famas/m249). The AWP's threshold is **10**, and pistols
/// penalise any velocity at all. 140 is the useful general number.
pub const ACCURATE_SPEED: f32 = 140.0;

/// Sideways speed while circling an opponent.
///
/// Below [`ACCURATE_SPEED`] on its own, so a bot that strafes and closes at the
/// same time is not automatically firing from the inaccurate branch.
pub const STRAFE_SPEED: f32 = 120.0;

/// How long one strafe direction is held, in seconds.
///
/// **Chosen.** Long enough to cover ground, short enough that the bot is not
/// predictable, and re-drawn per switch so two bots never sway in unison.
pub const STRAFE_MIN: f64 = 0.45;
pub const STRAFE_MAX: f64 = 1.15;

/// How often the per-target aim error is re-drawn, in seconds.
///
/// **Chosen.** Re-rolling every tick would be white noise, which averages to a
/// perfect aim over any burst and so makes the error free; re-rolling never
/// would be a fixed, learnable bias. A few hundred milliseconds reads as a hand
/// that is not quite steady.
pub const AIM_ERROR_REFRESH: f32 = 0.35;

/// Where the bot is going, at the two scales that matter.
///
/// Keeping them apart is not tidiness. "Have I arrived?" is a question about
/// the **objective**; "which way do I walk?" is a question about the **next
/// waypoint**. Collapsing the two into one `Option<Vec3>` is what let a bot
/// carrying the C4 reach a waypoint 2800 units from the bomb site, decide it
/// was standing on the plant spot, and stop dead -- every waypoint, all the way
/// across the map. Measured before the fix, on de_dust2:
///
/// ```text
/// rung plant       bomb true  to_goal 2868
/// rung plant-walk             to_goal 2848   (20 units in 8 seconds)
/// ```
///
/// An `Option<Vec3>` still converts into this as a goal with no waypoint, which
/// is the right reading for a caller that has no navigation layer.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Nav {
    /// The objective itself: the bomb site, the rescue zone.
    pub goal: Option<Vec3>,
    /// The next point to steer at on the way there. `None` means steer
    /// straight at the goal, which is only correct in an open room.
    pub waypoint: Option<Vec3>,
}

impl Nav {
    /// Head for `goal` with no route -- straight-line steering.
    pub fn to(goal: Vec3) -> Self {
        Self { goal: Some(goal), waypoint: None }
    }

    /// No objective at all.
    pub fn nowhere() -> Self {
        Self::default()
    }

    /// The point to actually turn toward this tick.
    pub fn steer(&self) -> Option<Vec3> {
        self.waypoint.or(self.goal)
    }
}

impl From<Option<Vec3>> for Nav {
    fn from(goal: Option<Vec3>) -> Self {
        Self { goal, waypoint: None }
    }
}

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
    /// Which way the bot is currently circling, and how long is left on it.
    strafe: (f32, f32),
    /// Where the brain wants to be routed, when that is not the bomb
    /// objective's target.
    ///
    /// The caller's navigation layer picks a destination for the route; left to
    /// itself it uses the map's declared objective, which on a hostage map is a
    /// hostage spawn and stays that way for the whole escort home. This is the
    /// brain telling it otherwise, one tick behind by construction — the same
    /// staleness the caller already accepts for `objective.target`.
    pub nav_goal: Option<Vec3>,
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
            strafe: (1.0, 0.0),
            nav_goal: None,
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
    ///
    /// The anti-idle drift is added **here**, on every path, rather than on the
    /// rungs that obviously stand still. `CheckActivityInGame` is an `&&` over
    /// both axes, and a bot walking a route across flat ground sweeps its yaw
    /// while holding its pitch exactly constant -- which scores as idle for as
    /// long as the walk lasts. Two rungs used to apply it and four did not, so
    /// a bot could be kicked, or made to drop the bomb by `afk_bomb_drop_time`
    /// (`dlls/player.cpp:4787-4792`), in the middle of doing its job.
    ///
    /// The drift is a fraction of a degree and moves far slower than the aim
    /// error already applied above it, so it costs nothing in a fight.
    fn wire_view(&self, world: &WorldView) -> Angles {
        let punch = predict_punch(
            world.me.punchangle,
            world.frametime,
            world.punch_prediction_frames(),
        );
        self.idle.apply(compensate(self.view, punch))
    }

    /// Head for `to`, expressed in the axes a `usercmd_t` actually carries.
    ///
    /// `view` must be the angle being **sent**, not the internal aim: the
    /// server builds the movement basis from the `viewangles` in the command it
    /// is executing, so decomposing against anything else walks the bot
    /// somewhere it did not ask to go.
    fn travel(&self, view: Angles, from: Vec3, to: Vec3, speed: f32) -> (f32, f32) {
        move_axes(view.yaw, aim_angles(from, to).yaw, speed)
    }

    /// Advance the circling timer and return the current side.
    ///
    /// Re-drawn on every switch rather than fixed, so a row of bots does not
    /// sway in step -- which is the sort of thing nobody notices until they see
    /// ten of them do it at once.
    fn strafe_side(&mut self, dt: f32) -> f32 {
        self.strafe.1 -= dt;
        if self.strafe.1 <= 0.0 {
            self.strafe.0 = -self.strafe.0;
            self.strafe.1 = self.rng.range(STRAFE_MIN, STRAFE_MAX) as f32;
        }
        self.strafe.0
    }

    /// Reset everything that describes a life or a specific weapon.
    fn reset_for_death(&mut self) {
        self.objective = ObjectiveState::default();
        self.plant.reset();
        self.defuse.reset();
        self.escort.reset();
        self.fire.reset();
        self.tracking = None;
        self.nav_goal = None;
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
    pub fn think(&mut self, world: &WorldView, nav: impl Into<Nav>, dt: f32) -> Intent {
        let nav = nav.into();
        self.idle.advance(dt);
        // Re-decided every tick, by whichever rung answers. Anything else
        // leaves a rung's destination in place after the ladder has moved on --
        // a bot that escorted a hostage last round and is defusing this one
        // would still be routed at a rescue zone.
        self.nav_goal = None;

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
            let view = self.wire_view(world);
            let mut intent = Intent::hold(view);
            intent.commands = self.buy_plan(world);
            self.rung = "freeze";
            return intent;
        }

        // Keep the navigation-level objective in step regardless of what the
        // ladder below decides — it tracks arrival and mode, not buttons.
        self.objective.tick(world, nav.goal, dt);

        // --- 3) A visible enemy -------------------------------------------
        //
        // ...unless we are carrying the bomb and can still walk away from it.
        // See CARRIER_HOLDS_FIRE_BEYOND: the carrier that stops to fight is the
        // reason a contested round never reaches a bomb site.
        let carrying = world.bomb.carried_by_me && !world.bomb.planted;
        let threat = select_target(world, &self.params).copied().filter(|t| {
            !carrying || distance2d(world.me.origin, t.origin) <= CARRIER_HOLDS_FIRE_BEYOND
        });
        if let Some(target) = threat {
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

            // Wanting the shot and being able to take it are different things.
            // Spread is chosen at the instant of PrimaryAttack from
            // `pev->velocity` and `FL_ONGROUND`, so a bot that fires while
            // still carrying its running speed throws the shot away -- and
            // zeroing the movement request does NOT zero the velocity, which
            // takes a few hundred milliseconds of friction to fall.
            //
            // Measured on a live four-bot run, sampled at every `rung combat`
            // tick: velocity above 140 in 60 of 119, above zero in 118 of 119,
            // airborne in 39 of 119. The accurate branch of the weapon in hand
            // was being taken essentially never.
            let weapon = world.me.weapon_or_unknown();
            let steady =
                world.me.on_ground && world.me.speed <= crate::fire::accurate_speed(weapon.id);
            let action = self.fire.decide(&weapon, want && steady);

            // A plant interrupted by a firefight is a plant that was released,
            // which the server has already cancelled. Record it honestly.
            if self.plant.is_arming() {
                self.plant.note_released();
            }

            self.rung = "combat";
            let view = self.wire_view(world);

            // Nobody walks in a straight line at someone who is shooting at
            // them, and here the human-looking answer and the effective one are
            // the same. Above ACCURATE_SPEED a rifle's spread jumps to the
            // moving branch, so the bot plants itself for the shot and circles
            // the rest of the time -- which is what a player does without
            // thinking about it.
            let side = self.strafe_side(dt);
            // Stop on `want`, not on `action.attack`. Gating the stop on the
            // shot that the stop is a precondition for is a deadlock: too fast
            // to fire, so never firing, so never stopping.
            let (forwardmove, sidemove) = if want {
                (0.0, 0.0)
            } else {
                let closing = if eng.advance { FORWARD_SPEED } else { 0.0 };
                let (f, s) =
                    self.travel(view, world.me.origin, target.origin, closing);
                (f, s + STRAFE_SPEED * side)
            };

            return Intent {
                view,
                forwardmove,
                sidemove,
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
            let view = self.wire_view(world);
            let (forwardmove, sidemove) = match defuse.move_to {
                Some(to) => self.travel(view, world.me.origin, to, FORWARD_SPEED),
                None => (0.0, 0.0),
            };
            return Intent {
                view,
                forwardmove,
                sidemove,
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
                // Arrival is measured against the bomb site; steering follows
                // the route to it. Once arrived the route is irrelevant and the
                // site itself is what to face.
                let steer = if arrived { target } else { nav.steer().unwrap_or(target) };
                self.view =
                    turn_toward(self.view, aim_angles(world.me.origin, steer), max_turn);

                // Only start the plant once actually there. `in_bomb_zone` is
                // the server's own permission bit, so it is tempting to let the
                // machine act on it alone -- but arming pins `maxspeed` to 1.0
                // (`CC4::GetMaxSpeed`, `wpn_c4.cpp:383-391`), so a bot that
                // begins the plant while still walking freezes itself in place
                // and never reaches the site. On a server with
                // `mp_plant_c4_anywhere` that is instant; on a normal one it
                // happens on the lip of the trigger.
                let out = if arrived {
                    self.plant.tick(world, dt)
                } else {
                    self.plant.reset();
                    crate::objective::bomb::PlantOutput::default()
                };
                let view = self.wire_view(world);
                // Standing still to plant is not optional: the server freezes
                // the planter anyway, and drifting out of the zone cancels the
                // whole thing.
                let (forwardmove, sidemove) = if arrived {
                    (0.0, 0.0)
                } else {
                    self.travel(view, world.me.origin, steer, FORWARD_SPEED)
                };
                let mut intent = Intent {
                    view,
                    forwardmove,
                    sidemove,
                    attack: out.attack,
                    move_target: Some(steer),
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
            // Publish where the escort wants to be, so the caller's navigation
            // layer routes THERE. Without this the route is computed to
            // whatever the map named as the objective -- on a hostage map, a
            // hostage spawn -- and stays pointed at it for the whole walk home,
            // which is the opposite direction. The bomb rungs never needed it
            // because `ObjectiveState::target` already said where they were
            // going; `ObjectiveState` is about the bomb and has no entry for an
            // escort at all.
            self.nav_goal = escort.goal;

            // Aim is the escort's business -- it looks at the point `PlayerUse`
            // measures its cone against, which is not what it is walking to.
            if let Some(look) = escort.look_at {
                self.view = turn_toward(self.view, aim_angles(world.me.origin, look), max_turn);
            }

            // Steering is the navigation layer's, and it is the route ALL the
            // way in -- unlike the plant rung, which switches to the site once
            // it has arrived. A bomb site is a floor you stand on; a hostage is
            // a thing that can be 90 units away and a storey up. Measured on
            // cs_italy with a 200-unit "close enough to walk straight at it"
            // shortcut in place: the bot reached `[900 2248 36]`, saw a hostage
            // 90 units off at z 160, walked at it, and spent the rest of the
            // round grinding into the underside of the staircase with 16
            // waypoints of a perfectly good route left unused. The route is
            // only abandoned when there is none -- the last few units into use
            // range are the follower's `None`, not a guess.
            let steer = escort.move_to.map(|to| nav.steer().unwrap_or(to));

            let speed = if escort.walk { WALK_SPEED } else { FORWARD_SPEED };
            self.rung = "hostage";
            let view = self.wire_view(world);
            let (forwardmove, sidemove) = match steer {
                Some(to) => self.travel(view, world.me.origin, to, speed),
                None => (0.0, 0.0),
            };
            return Intent {
                view,
                forwardmove,
                sidemove,
                use_action: escort.use_action,
                walk: escort.walk,
                move_target: steer,
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
        if let Some(t) = self.objective.target.or(nav.goal) {
            let arrived = distance2d(world.me.origin, t) < ARRIVE_RADIUS;
            let steer = if arrived { t } else { nav.steer().unwrap_or(t) };
            self.view = turn_toward(self.view, aim_angles(world.me.origin, steer), max_turn);
            self.rung = if arrived { "arrived" } else { "goto" };
            let view = self.wire_view(world);
            let (forwardmove, sidemove) = if arrived {
                (0.0, 0.0)
            } else {
                self.travel(view, world.me.origin, steer, FORWARD_SPEED)
            };
            return Intent {
                view,
                forwardmove,
                sidemove,
                move_target: Some(steer),
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
            view: self.wire_view(world),
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

    /// The bug this whole `Nav` split exists for.
    ///
    /// A bomb carrier crossing de_dust2 is always standing next to its next
    /// waypoint -- that is what a waypoint is. When the objective machine was
    /// fed the waypoint instead of the site, it kept answering "you are at the
    /// plant spot", so the bot stopped at every single one. It still made
    /// progress, at roughly 20 units per eight seconds, which is exactly slow
    /// enough to look like a navigation problem rather than an arrival one.
    #[test]
    fn standing_on_a_waypoint_is_not_standing_on_the_bomb_site() {
        let mut c = Controller::new(5, Difficulty::Normal);
        let site: Vec3 = [3000.0, 0.0, 0.0];
        let world = WorldView {
            me: SelfState {
                in_bomb_zone: true, // plant_c4_anywhere, or a generous trigger
                weapon: Some(WeaponState { id: WeaponId::C4, ..Default::default() }),
                ..me_at([0.0, 0.0, 0.0], Team::Terrorist)
            },
            bomb: BombState { carried_by_me: true, ..Default::default() },
            ..Default::default()
        };

        // Right on top of the next waypoint, and 3000 units from the site.
        let waypoint: Vec3 = [40.0, 0.0, 0.0];
        let nav = Nav { goal: Some(site), waypoint: Some(waypoint) };
        let intent = c.think(&world, nav, 0.1);

        assert!(intent.forwardmove > 0.0, "stopped 3000 units from the site");
        assert!(!intent.attack, "tried to plant 3000 units from the bomb site");
        assert_eq!(c.objective.objective, Objective::MoveToPlant);
        assert_eq!(intent.move_target, Some(waypoint), "steers at the waypoint");
    }

    /// ...and the reverse: an `Option` with no route still means "go there".
    #[test]
    fn a_bare_option_still_reads_as_a_goal_with_no_route() {
        let n: Nav = Some([1.0, 2.0, 3.0]).into();
        assert_eq!(n.goal, Some([1.0, 2.0, 3.0]));
        assert_eq!(n.waypoint, None);
        assert_eq!(n.steer(), Some([1.0, 2.0, 3.0]));
        assert_eq!(Nav::nowhere().steer(), None);
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

    /// Every outgoing angle must carry the anti-idle drift, not just the ones
    /// from the rungs that obviously stand still.
    ///
    /// `CheckActivityInGame` is an `&&` over both axes
    /// (`dlls/API/CSPlayer.cpp:539`), and a bot walking a route across flat
    /// ground sweeps its yaw while holding its pitch exactly constant -- which
    /// scores as idle for the whole walk. Two rungs applied the drift and four
    /// did not, so a bot could be dropped for idling, or made to let go of the
    /// bomb by `afk_bomb_drop_time`, in the middle of doing its job.
    #[test]
    fn every_rung_sends_a_view_that_keeps_moving_on_both_axes() {
        let site: Vec3 = [3000.0, 0.0, 0.0];
        let world = WorldView {
            me: me_at([0.0, 0.0, 0.0], Team::Terrorist),
            ..Default::default()
        };

        // Sampled the way the server samples: 5 seconds apart.
        for rung_world in [&world] {
            let mut c = Controller::new(9, Difficulty::Normal);
            let a = c.think(rung_world, Some(site), 0.05).view;
            assert_eq!(c.rung, "goto", "expected the walking rung");
            for _ in 0..100 {
                c.think(rung_world, Some(site), 0.05);
            }
            let b = c.think(rung_world, Some(site), 0.05).view;

            let dyaw = (a.yaw - b.yaw).abs();
            let dpitch = (a.pitch - b.pitch).abs();
            assert!(
                dyaw >= crate::idle::IDLE_ANGLE_EPSILON,
                "yaw moved {dyaw} in 5 s, needs {}",
                crate::idle::IDLE_ANGLE_EPSILON
            );
            assert!(
                dpitch >= crate::idle::IDLE_ANGLE_EPSILON,
                "pitch moved {dpitch} in 5 s -- an idle-kick on a walking bot",
            );
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
        // Sent pitch is the aim plus 2 * 3 degrees of down-correction, give or
        // take the anti-idle drift, which rides on every outgoing angle.
        let slack = c.idle.max_offset().pitch + 1e-3;
        assert!(
            (intent.view.pitch - (c.view.pitch + 6.0)).abs() <= slack,
            "sent {} vs internal {} (slack {slack})",
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

    /// Where a usercmd will ACTUALLY move the player, in world space.
    ///
    /// `forwardmove`/`sidemove` are meaningless without the view they are
    /// relative to, so any test that asserts on them alone is asserting on half
    /// a vector. This reassembles the engine's basis and reports the bearing
    /// and the speed, which are the two things the bot actually intends.
    fn travel_bearing(intent: &Intent) -> f32 {
        let y = f64::from(intent.view.yaw).to_radians();
        let (sy, cy) = y.sin_cos();
        let vx = cy * f64::from(intent.forwardmove) + sy * f64::from(intent.sidemove);
        let vy = sy * f64::from(intent.forwardmove) - cy * f64::from(intent.sidemove);
        vy.atan2(vx).to_degrees() as f32
    }

    fn travel_speed(intent: &Intent) -> f32 {
        intent.forwardmove.hypot(intent.sidemove)
    }

    fn bearing_to(from: Vec3, to: Vec3) -> f32 {
        aim_angles(from, to).yaw
    }

    /// The body goes where the route says, whatever the head is doing.
    ///
    /// Before the decomposition the bot pinned `forwardmove` to full speed and
    /// steered by turning, so it could only ever walk along its own crosshair.
    /// Since the view is turn-rate limited, that meant walking in a direction
    /// it had already decided against for the whole of every turn -- and it
    /// could never strafe, which is most of what makes movement look human.
    /// Nobody rounds a corner by rotating on the spot first.
    #[test]
    fn the_walk_goes_to_the_waypoint_even_while_the_view_is_catching_up() {
        let mut c = Controller::new(3, Difficulty::Normal);
        let world = WorldView {
            me: me_at([0.0, 0.0, 0.0], Team::CounterTerrorist),
            ..Default::default()
        };
        // Hard left of a bot looking down +X, so the turn cannot complete in
        // one tick at any sane turn rate.
        let waypoint: Vec3 = [0.0, 800.0, 0.0];

        let intent = c.think(&world, Some(waypoint), 0.05);
        assert_eq!(c.rung, "goto");
        assert!(
            intent.sidemove.abs() > 1.0,
            "a 90-degree turn produced no strafe at all: fwd {} side {}",
            intent.forwardmove,
            intent.sidemove
        );

        let want = bearing_to([0.0; 3], waypoint);
        let err = norm_angle(f64::from(travel_bearing(&intent) - want)).abs();
        assert!(err < 1.0, "walking {err:.1} degrees off the waypoint");

        let speed = travel_speed(&intent);
        assert!((speed - FORWARD_SPEED).abs() < 1.0, "speed {speed}");
    }

    /// Running at 250 u/s is not a firing position, and the bot must not
    /// deadlock waiting to be told otherwise.
    ///
    /// Spread is read from `pev->velocity` at the instant of PrimaryAttack, so
    /// firing mid-sprint throws the shot away. But the stop cannot be gated on
    /// the shot -- that is a deadlock: too fast to fire, so never firing, so
    /// never stopping. It is gated on WANTING the shot.
    #[test]
    fn a_sprinting_bot_holds_its_fire_but_plants_itself_to_take_the_shot() {
        let enemy = PlayerView {
            entity: 1,
            origin: [500.0, 0.0, 0.0],
            team: Team::CounterTerrorist,
            visible: true,
            ..Default::default()
        };
        let world = |speed: f32, on_ground: bool| WorldView {
            me: SelfState {
                can_shoot: true,
                speed,
                on_ground,
                weapon: Some(WeaponState {
                    id: WeaponId::Ak47,
                    clip: 30,
                    ..Default::default()
                }),
                ..me_at([0.0, 0.0, 0.0], Team::Terrorist)
            },
            players: vec![enemy],
            ..Default::default()
        };

        // Sprinting: never pulls the trigger, but does stop moving.
        let mut c = Controller::new(2, Difficulty::Unfair);
        let mut fired_while_fast = 0;
        let mut stopped = false;
        for _ in 0..200 {
            let i = c.think(&world(250.0, true), None, 0.05);
            if c.rung != "combat" {
                continue;
            }
            if i.attack {
                fired_while_fast += 1;
            }
            if i.forwardmove == 0.0 && i.sidemove == 0.0 {
                stopped = true;
            }
        }
        assert_eq!(fired_while_fast, 0, "fired {fired_while_fast} shots at a sprint");
        assert!(stopped, "never planted itself -- the stop is deadlocked on the shot");

        // Airborne is worse than moving, and is refused at any speed.
        let mut c = Controller::new(2, Difficulty::Unfair);
        let mut fired_airborne = 0;
        for _ in 0..200 {
            if c.think(&world(0.0, false), None, 0.05).attack {
                fired_airborne += 1;
            }
        }
        assert_eq!(fired_airborne, 0, "fired {fired_airborne} shots in mid-air");

        // Standing still on the ground: the shot is available.
        let mut c = Controller::new(2, Difficulty::Unfair);
        let mut fired = 0;
        for _ in 0..200 {
            if c.think(&world(0.0, true), None, 0.05).attack {
                fired += 1;
            }
        }
        assert!(fired > 0, "never fired even standing still on the ground");
    }

    /// The AWP's threshold is not the rifle's: 10 u/s against 140, a
    /// hundredfold spread penalty rather than a doubling
    /// (`wpn_awp.cpp:96-116`).
    #[test]
    fn the_awp_demands_a_dead_stop_where_a_rifle_tolerates_a_jog() {
        assert!(crate::fire::accurate_speed(WeaponId::Awp) < 20.0);
        assert!(crate::fire::accurate_speed(WeaponId::Ak47) > 100.0);
        assert!(crate::fire::accurate_speed(WeaponId::Deagle) < 50.0);
        assert!(crate::fire::accurate_speed(WeaponId::Scout) > 150.0);
    }

    /// The bomb carrier walks past a fight it can walk past.
    ///
    /// Ten bots, five a side, ten minutes: ZERO plants, the carrier killed
    /// first in every round -- while an unopposed one plants in about
    /// thirty-five seconds. A carrier that duels every CT it sees dies in the
    /// open and the round ends before anyone reaches a site.
    #[test]
    fn the_bomb_carrier_keeps_going_past_a_distant_enemy_but_fights_a_close_one() {
        let site: Vec3 = [3000.0, 0.0, 0.0];
        let world = |enemy_at: Vec3, carrying: bool| WorldView {
            me: SelfState {
                can_shoot: true,
                weapon: Some(WeaponState {
                    id: WeaponId::Ak47,
                    clip: 30,
                    ..Default::default()
                }),
                ..me_at([0.0, 0.0, 0.0], Team::Terrorist)
            },
            players: vec![PlayerView {
                entity: 1,
                origin: enemy_at,
                team: Team::CounterTerrorist,
                visible: true,
                ..Default::default()
            }],
            bomb: BombState { carried_by_me: carrying, ..Default::default() },
            ..Default::default()
        };

        let far = [CARRIER_HOLDS_FIRE_BEYOND + 200.0, 0.0, 0.0];
        let near = [CARRIER_HOLDS_FIRE_BEYOND - 200.0, 0.0, 0.0];

        // Not carrying: a distant enemy is still a fight.
        let mut c = Controller::new(1, Difficulty::Normal);
        c.think(&world(far, false), Some(site), 0.1);
        assert_eq!(c.rung, "combat", "a normal bot engages at that range");

        // Carrying, and it can walk away: it walks.
        let mut c = Controller::new(1, Difficulty::Normal);
        c.think(&world(far, true), Some(site), 0.1);
        assert_ne!(c.rung, "combat", "the carrier stopped for a fight it could leave");

        // Carrying, but the enemy is on top of it: running is not an option.
        let mut c = Controller::new(1, Difficulty::Normal);
        c.think(&world(near, true), Some(site), 0.1);
        assert_eq!(c.rung, "combat", "the carrier ignored an enemy at close range");
    }

    /// Circling an opponent, but planting to shoot.
    ///
    /// Above ACCURATE_SPEED a rifle's spread jumps to the moving branch
    /// (`wpn_ak47.cpp:75-86`), so standing still for the shot is both what a
    /// player does and what actually hits.
    #[test]
    fn a_bot_in_a_firefight_circles_but_stops_to_shoot() {
        let mut c = Controller::new(11, Difficulty::Easy);
        let w = WorldView {
            me: SelfState {
                can_shoot: true,
                weapon: Some(WeaponState { id: WeaponId::Ak47, clip: 30, ..Default::default() }),
                ..me_at([0.0, 0.0, 0.0], Team::Terrorist)
            },
            players: vec![PlayerView {
                entity: 1,
                origin: [600.0, 0.0, 0.0],
                team: Team::CounterTerrorist,
                visible: true,
                ..Default::default()
            }],
            ..Default::default()
        };

        let mut circled = false;
        let mut planted_to_shoot = false;
        for _ in 0..400 {
            let i = c.think(&w, None, 0.05);
            if c.rung != "combat" {
                continue;
            }
            if i.attack {
                planted_to_shoot |= travel_speed(&i) == 0.0;
            } else if i.sidemove.abs() > 1.0 {
                circled = true;
            }
        }
        assert!(circled, "walked at the enemy in a straight line");
        assert!(planted_to_shoot, "never stopped moving to take a shot");
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
        // The zone is directly BEHIND the bot, and the view is turn-rate
        // limited, so on this tick it moves there sideways/backwards while the
        // head comes round. That is the point of the decomposition: the body
        // goes where it was told regardless of where the crosshair has got to.
        let speed = travel_speed(&intent);
        assert!(speed > 0.0 && speed <= WALK_SPEED, "speed {speed}");
        assert!(WALK_SPEED < 150.0, "and stay under the footstep threshold");
        let want = bearing_to([0.0; 3], [-2000.0, 0.0, 0.0]);
        let err = norm_angle(f64::from(travel_bearing(&intent) - want)).abs();
        assert!(err < 1.0, "walking {err:.1} degrees off the rescue zone");
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
