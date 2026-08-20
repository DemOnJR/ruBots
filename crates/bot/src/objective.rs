//! Bomb objective — planting and defusing.
//!
//! Port of `commitObjective` / `status` from `internal/bot/think.go`.
//!
//! Verified from `commitObjective` (`0x1406FFD00`): it aims at a spot, turns
//! toward it, issues a console command, and logs, in order,
//! `"planting the bomb"`, `"plant spot not valid - moving on"`,
//! `"bomb planted successfully"` and `"defusing the bomb"`. It sends the
//! literal `"w"` — the bot holds the use/attack key by console command rather
//! than by button bit.
//!
//! The float pool it references is `12.0`, `72.0`, `100.0` and `160.0`. Their
//! precise roles were not established, so they are recorded in
//! [`RECOVERED_OBJECTIVE_CONSTANTS`] and the radii below are named after what
//! they plainly must gate — reaching the spot, and staying on it.
//!
//! ## What this layer is, after the rework
//!
//! This module now does **navigation and mode selection only**: which of the
//! bomb objectives applies, and where to stand for it. The button patterns and
//! the completion conditions live in [`bomb`] and [`hostage`], which are driven
//! by observation rather than by a private stopwatch.
//!
//! The one behavioural change here is the same principle applied to the
//! completion event: a plant used to be declared finished when a local timer
//! reached 3 seconds. It is now declared finished when **the server says the
//! bomb is planted**. Holding the button for three seconds is not evidence the
//! plant succeeded — it is exactly what a cancelled plant also looks like from
//! the inside. [`ObjectiveState::elapsed`] still counts, because "how long have
//! I been holding my own button" is genuinely local knowledge; it just no
//! longer decides anything.

pub mod bomb;
pub mod hostage;

use crate::math::{distance2d, Vec3};
use crate::world::{Team, WorldView};

/// Floats `commitObjective` compares against, recovered from `.rdata`.
pub const RECOVERED_OBJECTIVE_CONSTANTS: [f64; 4] = [12.0, 72.0, 100.0, 160.0];

/// How close the bot must be to start planting/defusing.
///
/// 72 units is the largest of the recovered "small" constants and is the
/// conventional CS use-radius; treat as inferred, not verified.
pub const ACTION_RADIUS: f32 = 72.0;

/// Drifting further than this abandons the action.
pub const ABANDON_RADIUS: f32 = 160.0;

/// How far a terrorist will go out of its way for a dropped bomb.
///
/// **Chosen.** It has to be a radius rather than "always", or a whole team
/// abandons the site for one object the moment its carrier dies. Roughly a
/// third of the long diagonal of de_dust2, so the two or three players nearest the
/// body react and the rest keep pushing.
pub const RETRIEVE_RADIUS: f32 = 1400.0;

/// Seconds the plant animation takes — `C4_ARMING_ON_TIME`,
/// `dlls/weapons.h:860`. Verified, unlike the radii above.
pub const PLANT_DURATION: f32 = bomb::C4_ARMING_ON_TIME;
/// Defuse time without a kit (`dlls/ggrenade.cpp:1067`).
pub const DEFUSE_DURATION: f32 = bomb::DEFUSE_TIME_NO_KIT;
/// Defuse time with a kit (`dlls/ggrenade.cpp:1052`).
pub const DEFUSE_DURATION_KIT: f32 = bomb::DEFUSE_TIME_KIT;

/// What the bot is doing about the bomb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Objective {
    /// Nothing to do about the bomb right now.
    Idle,
    /// Move to `target` in order to plant.
    MoveToPlant,
    /// Standing on the site, planting.
    Planting,
    /// Go and pick a dropped bomb up off the ground.
    RetrieveBomb,
    /// Move to the planted bomb.
    MoveToDefuse,
    /// Standing on the bomb, defusing.
    Defusing,
    /// Phase G4: T post-plant — hold an entry angle near the bomb (not Idle hunt).
    DefendPlant,
}

/// Objective progress across ticks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ObjectiveState {
    pub objective: Objective,
    /// Where we are heading, when we have a destination.
    pub target: Option<Vec3>,
    /// Seconds spent in the current action.
    pub elapsed: f32,
}

impl Default for ObjectiveState {
    fn default() -> Self {
        Self {
            objective: Objective::Idle,
            target: None,
            elapsed: 0.0,
        }
    }
}

/// A log line the original emits, so behaviour can be traced the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectiveEvent {
    PlantingTheBomb,
    PlantSpotNotValid,
    BombPlantedSuccessfully,
    DefusingTheBomb,
}

impl ObjectiveEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PlantingTheBomb => "planting the bomb",
            Self::PlantSpotNotValid => "plant spot not valid - moving on",
            Self::BombPlantedSuccessfully => "bomb planted successfully",
            Self::DefusingTheBomb => "defusing the bomb",
        }
    }
}

/// How long a defuse takes for this bot.
pub fn defuse_time(has_kit: bool) -> f32 {
    if has_kit {
        DEFUSE_DURATION_KIT
    } else {
        DEFUSE_DURATION
    }
}

/// Phase G4: T post-plant hold offset from the bomb model.
///
/// Real teams do not stack on the C4. Four entry-denial lanes around the plant
/// (plus one closer "close" for defuse denial) give different bots different
/// doors/angles. Salt is stable for a still body (rounded origin) so the hold
/// does not jitter every tick.
pub fn t_post_plant_hold(bomb: Vec3, me: &crate::world::SelfState) -> Vec3 {
    // Quantise origin so the lane does not flip while walking a few units.
    let qx = (me.origin[0] / 64.0).floor() as i32;
    let qy = (me.origin[1] / 64.0).floor() as i32;
    let salt = (qx.wrapping_mul(0x45d9_f3b) ^ qy.wrapping_mul(0x27d4_eb2d)) as u32;
    let lane = salt % 5;
    // Bearings: N / E / S / W / NE — covers typical dual entrances on dust2 sites.
    let angle = match lane {
        0 => 0.0f32,
        1 => std::f32::consts::FRAC_PI_2,
        2 => std::f32::consts::PI,
        3 => -std::f32::consts::FRAC_PI_2,
        _ => std::f32::consts::FRAC_PI_4,
    };
    // Lane 4 (close) sits ~180u on the bomb for defuse denial; others 320–480u.
    let r = if lane == 4 {
        180.0
    } else {
        320.0 + (salt % 5) as f32 * 32.0
    };
    let (s, c) = angle.sin_cos();
    [bomb[0] + r * c, bomb[1] + r * s, bomb[2]]
}

impl ObjectiveState {
    /// Advance the objective by one tick.
    ///
    /// `site` is the nearest bomb-site position (a `GOAL` node in the nav
    /// graph); `dt` is the tick duration in seconds.
    pub fn tick(
        &mut self,
        world: &WorldView,
        site: Option<Vec3>,
        dt: f32,
    ) -> Option<ObjectiveEvent> {
        if !world.me.alive {
            *self = Self::default();
            return None;
        }

        // The only confirmation a plant ever gets: the server said so. This has
        // to be checked before the defuse branch below, or the transition out
        // of `Planting` would be swallowed by "a bomb is planted, go defuse it".
        if self.objective == Objective::Planting && world.bomb.planted {
            self.objective = Objective::Idle;
            self.elapsed = 0.0;
            return Some(ObjectiveEvent::BombPlantedSuccessfully);
        }

        // Defusing / post-plant defence: a planted bomb is the whole game.
        if world.bomb.planted {
            // Phase G4 — terrorists do NOT go Idle and wander. They hold
            // seed-stable offsets around the bomb (entry denial), so CT cannot
            // walk onto site free. One lane sits closer for defuse denial.
            if world.me.team == Team::Terrorist {
                let bomb = world.bomb.origin.or(site);
                if let Some(bomb) = bomb {
                    let hold = t_post_plant_hold(bomb, &world.me);
                    self.objective = Objective::DefendPlant;
                    self.target = Some(hold);
                    self.elapsed += dt;
                    return None;
                }
                self.objective = Objective::Idle;
                self.target = None;
                return None;
            }
            if world.me.team != Team::CounterTerrorist {
                self.objective = Objective::Idle;
                self.target = None;
                return None;
            }
            // CT retake: prefer decoded plant origin; fall back to map site so
            // bots who never saw the plant entity still path to the right half.
            let Some(bomb) = world.bomb.origin.or(site) else {
                self.objective = Objective::Idle;
                return None;
            };
            let d = distance2d(world.me.origin, bomb);
            self.target = Some(bomb);

            return match self.objective {
                Objective::Defusing => {
                    if d > ABANDON_RADIUS {
                        self.objective = Objective::MoveToDefuse;
                        self.elapsed = 0.0;
                        None
                    } else {
                        self.elapsed += dt;
                        None
                    }
                }
                _ => {
                    if d <= ACTION_RADIUS {
                        self.objective = Objective::Defusing;
                        self.elapsed = 0.0;
                        Some(ObjectiveEvent::DefusingTheBomb)
                    } else {
                        self.objective = Objective::MoveToDefuse;
                        None
                    }
                }
            };
        }

        // Not planted, and not ours: it is lying on the ground where its
        // carrier died. Somebody has to go and get it.
        //
        // Without this the round is decided the moment the bomb runner is
        // killed. Measured over four rounds of a 10v10: four carriers spawned
        // with the bomb, all four killed about twenty seconds in, zero plants,
        // every round ending Target_Saved.
        //
        // Only terrorists are told about it -- `BombDrop` on a death goes
        // MSG_ONE to each live terrorist (`dlls/player.cpp:8494-8499`) -- so
        // this is exactly the team that can act on it, and a CT that somehow
        // saw the message still has no use for it.
        if !world.bomb.carried_by_me || world.me.team != Team::Terrorist {
            let retrieving = world.me.team == Team::Terrorist
                && !world.bomb.carried_by_me
                && world
                    .bomb
                    .origin
                    .is_some_and(|b| distance2d(world.me.origin, b) <= RETRIEVE_RADIUS);
            if retrieving {
                // Picking it up is a touch, not an action: walking over a
                // dropped C4 gives it to a terrorist, and the server then tells
                // everyone with `BombPickup`. So there is nothing to press --
                // the whole objective is to stand on it.
                self.objective = Objective::RetrieveBomb;
                self.target = world.bomb.origin;
                self.elapsed += dt;
                return None;
            }
            if self.objective != Objective::Idle {
                *self = Self::default();
            }
            return None;
        }

        let Some(site) = site else {
            self.objective = Objective::Idle;
            self.target = None;
            return None;
        };
        self.target = Some(site);
        let d = distance2d(world.me.origin, site);

        match self.objective {
            Objective::Planting => {
                if d > ABANDON_RADIUS {
                    self.objective = Objective::MoveToPlant;
                    self.elapsed = 0.0;
                    return Some(ObjectiveEvent::PlantSpotNotValid);
                }
                // Keep holding. Completion is observed at the top of `tick`,
                // never inferred from this counter.
                self.elapsed += dt;
                None
            }
            _ => {
                if d <= ACTION_RADIUS {
                    self.objective = Objective::Planting;
                    self.elapsed = 0.0;
                    Some(ObjectiveEvent::PlantingTheBomb)
                } else {
                    self.objective = Objective::MoveToPlant;
                    None
                }
            }
        }
    }

    /// True while the bot must stand still and hold the action key.
    pub fn is_acting(&self) -> bool {
        matches!(self.objective, Objective::Planting | Objective::Defusing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{BombState, SelfState};

    const SITE: Vec3 = [1000.0, 1000.0, 0.0];

    fn terrorist_with_bomb(at: Vec3) -> WorldView {
        WorldView {
            me: SelfState {
                origin: at,
                team: Team::Terrorist,
                ..Default::default()
            },
            bomb: BombState {
                carried_by_me: true,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn ct_near_planted_bomb(at: Vec3, kit: bool) -> WorldView {
        WorldView {
            me: SelfState {
                origin: at,
                team: Team::CounterTerrorist,
                has_defuse_kit: kit,
                ..Default::default()
            },
            bomb: BombState {
                planted: true,
                origin: Some(SITE),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn carrier_walks_to_the_site_then_plants() {
        let mut s = ObjectiveState::default();
        let far = terrorist_with_bomb([0.0, 0.0, 0.0]);
        assert_eq!(s.tick(&far, Some(SITE), 0.1), None);
        assert_eq!(s.objective, Objective::MoveToPlant);
        assert_eq!(s.target, Some(SITE));

        let there = terrorist_with_bomb([1000.0, 1010.0, 0.0]);
        assert_eq!(
            s.tick(&there, Some(SITE), 0.1),
            Some(ObjectiveEvent::PlantingTheBomb)
        );
        assert!(s.is_acting());
    }

    #[test]
    fn planting_completes_only_when_the_server_confirms_it() {
        let mut s = ObjectiveState::default();
        let there = terrorist_with_bomb([1000.0, 1000.0, 0.0]);
        s.tick(&there, Some(SITE), 0.1); // begins planting

        // Ten seconds of holding — more than three times the arming time —
        // proves nothing on its own. A cancelled plant looks exactly like this
        // from the inside, which is why the old local timer was wrong.
        for _ in 0..100 {
            assert_eq!(
                s.tick(&there, Some(SITE), 0.1),
                None,
                "a private stopwatch must never declare the bomb planted"
            );
        }
        assert!(s.elapsed >= PLANT_DURATION, "it did keep holding, though");
        assert_eq!(s.objective, Objective::Planting);

        // The server saying so is the only thing that finishes it.
        let mut planted = there;
        planted.bomb.planted = true;
        assert_eq!(
            s.tick(&planted, Some(SITE), 0.1),
            Some(ObjectiveEvent::BombPlantedSuccessfully)
        );
        assert_eq!(s.objective, Objective::Idle);
        assert_eq!(s.elapsed, 0.0);
    }

    #[test]
    fn walking_off_the_spot_aborts_the_plant() {
        let mut s = ObjectiveState::default();
        let there = terrorist_with_bomb([1000.0, 1000.0, 0.0]);
        s.tick(&there, Some(SITE), 0.1);
        assert_eq!(s.objective, Objective::Planting);

        let wandered = terrorist_with_bomb([1400.0, 1000.0, 0.0]);
        assert_eq!(
            s.tick(&wandered, Some(SITE), 0.1),
            Some(ObjectiveEvent::PlantSpotNotValid)
        );
        assert_eq!(s.objective, Objective::MoveToPlant);
    }

    #[test]
    fn a_terrorist_without_the_bomb_does_not_plant() {
        let mut s = ObjectiveState::default();
        let mut w = terrorist_with_bomb([1000.0, 1000.0, 0.0]);
        w.bomb.carried_by_me = false;
        assert_eq!(s.tick(&w, Some(SITE), 0.1), None);
        assert_eq!(s.objective, Objective::Idle);
    }

    #[test]
    fn cts_defuse_a_planted_bomb() {
        let mut s = ObjectiveState::default();
        let far = ct_near_planted_bomb([0.0, 0.0, 0.0], false);
        assert_eq!(s.tick(&far, None, 0.1), None);
        assert_eq!(s.objective, Objective::MoveToDefuse);

        let there = ct_near_planted_bomb([1000.0, 1020.0, 0.0], false);
        assert_eq!(
            s.tick(&there, None, 0.1),
            Some(ObjectiveEvent::DefusingTheBomb)
        );
        assert!(s.is_acting());
    }

    #[test]
    fn terrorists_do_not_defuse_their_own_bomb() {
        let mut s = ObjectiveState::default();
        let mut w = ct_near_planted_bomb([1000.0, 1000.0, 0.0], false);
        w.me.team = Team::Terrorist;
        assert_eq!(s.tick(&w, None, 0.1), None);
        // G4: T post-plant defends, never defuses.
        assert_eq!(s.objective, Objective::DefendPlant);
        assert!(s.target.is_some());
        let hold = s.target.unwrap();
        let d = distance2d(SITE, hold);
        assert!(
            d >= 150.0 && d <= 550.0,
            "hold should be offset from bomb, got d={d}"
        );
    }

    #[test]
    fn a_planted_bomb_outranks_planting() {
        // Even holding the bomb, if it is somehow already planted the carrier
        // stops trying to plant and switches to post-plant defence (G4).
        let mut s = ObjectiveState::default();
        let mut w = terrorist_with_bomb([1000.0, 1000.0, 0.0]);
        w.bomb.planted = true;
        w.bomb.origin = Some(SITE);
        assert_eq!(s.tick(&w, Some(SITE), 0.1), None);
        assert_eq!(s.objective, Objective::DefendPlant);
        assert!(s.target.is_some());
    }

    #[test]
    fn t_post_plant_holds_fan_out() {
        let bomb = SITE;
        let mut cells = std::collections::HashSet::new();
        for i in 0..12 {
            let mut me = crate::world::SelfState::default();
            me.team = Team::Terrorist;
            // Spread "identities" via quantised origin buckets.
            me.origin = [i as f32 * 200.0, (i % 3) as f32 * 200.0, 0.0];
            let h = t_post_plant_hold(bomb, &me);
            cells.insert(((h[0] / 80.0).floor() as i32, (h[1] / 80.0).floor() as i32));
            let d = distance2d(bomb, h);
            assert!((150.0..550.0).contains(&d), "lane dist {d}");
        }
        assert!(
            cells.len() >= 3,
            "12 T identities should use several hold cells, got {}",
            cells.len()
        );
    }

    #[test]
    fn dying_resets_the_objective() {
        let mut s = ObjectiveState::default();
        let mut w = terrorist_with_bomb([1000.0, 1000.0, 0.0]);
        s.tick(&w, Some(SITE), 0.1);
        assert_eq!(s.objective, Objective::Planting);

        w.me.alive = false;
        s.tick(&w, Some(SITE), 0.1);
        assert_eq!(s.objective, Objective::Idle);
        assert_eq!(s.elapsed, 0.0);
    }

    #[test]
    fn no_known_site_means_no_plant() {
        let mut s = ObjectiveState::default();
        let w = terrorist_with_bomb([1000.0, 1000.0, 0.0]);
        assert_eq!(s.tick(&w, None, 0.1), None);
        assert_eq!(s.objective, Objective::Idle);
    }

    #[test]
    fn a_kit_halves_the_defuse_time() {
        assert!(defuse_time(true) < defuse_time(false));
        assert_eq!(defuse_time(true), 5.0);
        assert_eq!(defuse_time(false), 10.0);
    }

    #[test]
    fn event_strings_match_the_recovered_literals() {
        assert_eq!(
            ObjectiveEvent::PlantingTheBomb.as_str(),
            "planting the bomb"
        );
        assert_eq!(
            ObjectiveEvent::PlantSpotNotValid.as_str(),
            "plant spot not valid - moving on"
        );
        assert_eq!(
            ObjectiveEvent::BombPlantedSuccessfully.as_str(),
            "bomb planted successfully"
        );
        assert_eq!(
            ObjectiveEvent::DefusingTheBomb.as_str(),
            "defusing the bomb"
        );
    }

    /// A dropped bomb is not somebody else's problem.
    ///
    /// Measured over four rounds of a live 10v10: four carriers spawned with
    /// the bomb, all four were killed about twenty seconds in, the bomb lay
    /// where each of them fell, and every round ended Target_Saved. Nothing in
    /// the objective machine reacted to a bomb on the ground.
    #[test]
    fn a_terrorist_goes_and_picks_up_a_dropped_bomb() {
        let bomb: Vec3 = [500.0, 0.0, 0.0];
        let mut w = terrorist_with_bomb([0.0, 0.0, 0.0]);
        w.bomb.carried_by_me = false;
        w.bomb.planted = false;
        w.bomb.origin = Some(bomb);

        let mut o = ObjectiveState::default();
        o.tick(&w, Some(SITE), 0.1);
        assert_eq!(o.objective, Objective::RetrieveBomb);
        assert_eq!(o.target, Some(bomb), "must head for the bomb, not the site");
    }

    /// ...but not from the other side of the map, or one dead carrier pulls
    /// the whole team off the objective.
    #[test]
    fn a_bomb_too_far_away_is_left_for_somebody_closer() {
        let mut w = terrorist_with_bomb([0.0, 0.0, 0.0]);
        w.bomb.carried_by_me = false;
        w.bomb.origin = Some([RETRIEVE_RADIUS + 200.0, 0.0, 0.0]);

        let mut o = ObjectiveState::default();
        o.tick(&w, Some(SITE), 0.1);
        assert_ne!(o.objective, Objective::RetrieveBomb);
    }

    /// A counter-terrorist has no use for it.
    #[test]
    fn a_ct_does_not_chase_an_unplanted_bomb() {
        let mut w = terrorist_with_bomb([0.0, 0.0, 0.0]);
        w.me.team = Team::CounterTerrorist;
        w.bomb.carried_by_me = false;
        w.bomb.origin = Some([300.0, 0.0, 0.0]);

        let mut o = ObjectiveState::default();
        o.tick(&w, Some(SITE), 0.1);
        assert_eq!(o.objective, Objective::Idle);
    }

    /// Carrying it again outranks going to get it.
    #[test]
    fn picking_it_up_switches_straight_back_to_planting() {
        let mut w = terrorist_with_bomb([0.0, 0.0, 0.0]);
        w.bomb.carried_by_me = false;
        w.bomb.origin = Some([300.0, 0.0, 0.0]);
        let mut o = ObjectiveState::default();
        o.tick(&w, Some(SITE), 0.1);
        assert_eq!(o.objective, Objective::RetrieveBomb);

        w.bomb.carried_by_me = true;
        o.tick(&w, Some(SITE), 0.1);
        assert_eq!(o.objective, Objective::MoveToPlant);
        assert_eq!(o.target, Some(SITE));
    }
}
