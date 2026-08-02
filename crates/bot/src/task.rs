//! What the bot is currently trying to do.
//!
//! Port of the task model in `internal/bot/navigate.go` (`chooseTask`) and
//! `internal/bot/difficulty.go`.
//!
//! The task names are string literals recovered from `chooseTask`
//! (`0x1406FBDC0`) and its neighbours: `guard`, `weapons`, `roam`, `push`,
//! `plant`, `defuse`, `hunt`, `chase`, `camp`.

use std::fmt;

/// The bot's current objective.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Task {
    /// Hold a defensive node.
    Guard,
    /// Head for a weapon/buy opportunity.
    Weapons,
    /// No better idea — wander.
    Roam,
    /// Advance on the objective.
    Push,
    /// Plant the bomb.
    Plant,
    /// Defuse the bomb.
    Defuse,
    /// Look for enemies.
    Hunt,
    /// Pursue a specific enemy.
    Chase,
    /// Sit still and watch an area.
    Camp,
}

impl Task {
    /// The literal the original logs for this task.
    pub fn as_str(self) -> &'static str {
        match self {
            Task::Guard => "guard",
            Task::Weapons => "weapons",
            Task::Roam => "roam",
            Task::Push => "push",
            Task::Plant => "plant",
            Task::Defuse => "defuse",
            Task::Hunt => "hunt",
            Task::Chase => "chase",
            Task::Camp => "camp",
        }
    }

    /// Tasks that must not be interrupted by ordinary re-planning: abandoning
    /// a plant or defuse halfway is worse than finishing it.
    pub fn is_committed(self) -> bool {
        matches!(self, Task::Plant | Task::Defuse)
    }

    /// Tasks that involve moving toward somewhere.
    pub fn is_moving(self) -> bool {
        matches!(
            self,
            Task::Roam | Task::Push | Task::Plant | Task::Defuse | Task::Hunt | Task::Chase
        )
    }
}

impl fmt::Display for Task {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Skill level. The names are the `Select` options recovered from the GUI
/// (`easy`, `normal`, `hard`, `unfair`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Difficulty {
    Easy,
    Normal,
    Hard,
    Unfair,
}

impl Difficulty {
    pub fn as_str(self) -> &'static str {
        match self {
            Difficulty::Easy => "easy",
            Difficulty::Normal => "normal",
            Difficulty::Hard => "hard",
            Difficulty::Unfair => "unfair",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "easy" => Difficulty::Easy,
            "normal" => Difficulty::Normal,
            "hard" => Difficulty::Hard,
            "unfair" => Difficulty::Unfair,
            _ => return None,
        })
    }

    /// Per-tick turn cap fed to [`crate::aim::turn_toward`].
    ///
    /// **Not yet extracted.** `bot.map.init.0` (`internal/bot/difficulty.go`)
    /// holds the real per-difficulty table; these are placeholders chosen to
    /// bracket the verified default of 20.0 so behaviour is plausible until
    /// the table is read out. Only the `Normal` value is known to match the
    /// binary.
    pub fn max_turn(self) -> f64 {
        match self {
            Difficulty::Easy => 8.0,
            Difficulty::Normal => crate::aim::DEFAULT_MAX_TURN,
            Difficulty::Hard => 35.0,
            Difficulty::Unfair => 90.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_names_match_the_recovered_literals() {
        assert_eq!(Task::Guard.as_str(), "guard");
        assert_eq!(Task::Weapons.as_str(), "weapons");
        assert_eq!(Task::Roam.as_str(), "roam");
        assert_eq!(Task::Push.as_str(), "push");
        assert_eq!(Task::Plant.as_str(), "plant");
    }

    #[test]
    fn objective_tasks_are_committed() {
        assert!(Task::Plant.is_committed());
        assert!(Task::Defuse.is_committed());
        assert!(!Task::Roam.is_committed());
        assert!(!Task::Camp.is_committed());
    }

    #[test]
    fn camping_and_guarding_are_stationary() {
        assert!(!Task::Camp.is_moving());
        assert!(!Task::Guard.is_moving());
        assert!(Task::Chase.is_moving());
    }

    #[test]
    fn difficulty_round_trips_through_its_name() {
        for d in [
            Difficulty::Easy,
            Difficulty::Normal,
            Difficulty::Hard,
            Difficulty::Unfair,
        ] {
            assert_eq!(Difficulty::parse(d.as_str()), Some(d));
        }
        assert_eq!(Difficulty::parse("impossible"), None);
    }

    #[test]
    fn harder_bots_turn_faster() {
        assert!(Difficulty::Easy.max_turn() < Difficulty::Normal.max_turn());
        assert!(Difficulty::Normal.max_turn() < Difficulty::Hard.max_turn());
        assert!(Difficulty::Hard.max_turn() < Difficulty::Unfair.max_turn());
    }

    #[test]
    fn normal_matches_the_verified_engine_default() {
        assert_eq!(Difficulty::Normal.max_turn(), 20.0);
    }
}
