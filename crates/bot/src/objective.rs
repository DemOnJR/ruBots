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

/// Seconds the plant animation takes in CS 1.6.
pub const PLANT_DURATION: f32 = 3.0;
/// Defuse time without a kit.
pub const DEFUSE_DURATION: f32 = 10.0;
/// Defuse time with a kit.
pub const DEFUSE_DURATION_KIT: f32 = 5.0;

/// What the bot is doing about the bomb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Objective {
    /// Nothing to do about the bomb right now.
    Idle,
    /// Move to `target` in order to plant.
    MoveToPlant,
    /// Standing on the site, planting.
    Planting,
    /// Move to the planted bomb.
    MoveToDefuse,
    /// Standing on the bomb, defusing.
    Defusing,
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
        Self { objective: Objective::Idle, target: None, elapsed: 0.0 }
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

        // Defusing takes precedence: a planted bomb is the whole game.
        if world.bomb.planted {
            if world.me.team != Team::CounterTerrorist {
                self.objective = Objective::Idle;
                self.target = None;
                return None;
            }
            let Some(bomb) = world.bomb.origin else {
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

        // Not planted: only the carrier plants.
        if !world.bomb.carried_by_me || world.me.team != Team::Terrorist {
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
                self.elapsed += dt;
                if self.elapsed >= PLANT_DURATION {
                    self.objective = Objective::Idle;
                    self.elapsed = 0.0;
                    return Some(ObjectiveEvent::BombPlantedSuccessfully);
                }
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
            me: SelfState { origin: at, team: Team::Terrorist, ..Default::default() },
            bomb: BombState { carried_by_me: true, ..Default::default() },
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
            bomb: BombState { planted: true, origin: Some(SITE), carried_by_me: false },
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
    fn planting_completes_after_the_plant_duration() {
        let mut s = ObjectiveState::default();
        let there = terrorist_with_bomb([1000.0, 1000.0, 0.0]);
        s.tick(&there, Some(SITE), 0.1); // begins planting

        let mut done = None;
        for _ in 0..100 {
            if let Some(e) = s.tick(&there, Some(SITE), 0.1) {
                done = Some(e);
                break;
            }
        }
        assert_eq!(done, Some(ObjectiveEvent::BombPlantedSuccessfully));
        assert_eq!(s.objective, Objective::Idle);
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
        assert_eq!(s.objective, Objective::Idle);
    }

    #[test]
    fn a_planted_bomb_outranks_planting() {
        // Even holding the bomb, if it is somehow already planted the carrier
        // stops trying to plant.
        let mut s = ObjectiveState::default();
        let mut w = terrorist_with_bomb([1000.0, 1000.0, 0.0]);
        w.bomb.planted = true;
        w.bomb.origin = Some(SITE);
        assert_eq!(s.tick(&w, Some(SITE), 0.1), None);
        assert_eq!(s.objective, Objective::Idle);
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
        assert_eq!(ObjectiveEvent::PlantingTheBomb.as_str(), "planting the bomb");
        assert_eq!(
            ObjectiveEvent::PlantSpotNotValid.as_str(),
            "plant spot not valid - moving on"
        );
        assert_eq!(
            ObjectiveEvent::BombPlantedSuccessfully.as_str(),
            "bomb planted successfully"
        );
        assert_eq!(ObjectiveEvent::DefusingTheBomb.as_str(), "defusing the bomb");
    }
}
