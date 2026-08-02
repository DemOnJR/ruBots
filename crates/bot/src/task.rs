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

    /// Per-tick turn cap in degrees, fed to [`crate::aim::turn_toward`].
    ///
    /// **Chosen, not recovered** — with one exception. `bot.map.init.0`
    /// (`internal/bot/difficulty.go`) holds the original's per-difficulty
    /// table and it has not been read out; only `Normal` is pinned, because
    /// 20.0 is the verified default in `(*Bot).turn` itself
    /// (`0x141149170`). The other three are picked to bracket it on a roughly
    /// geometric ladder, so each step up is a visible difference rather than a
    /// nudge.
    ///
    /// This and the three functions below are one coherent set: they are meant
    /// to be read and tuned together, because turning fast while aiming badly
    /// (or vice versa) does not read as a difficulty, it reads as a bug.
    pub fn max_turn(self) -> f64 {
        match self {
            Difficulty::Easy => 6.0,
            Difficulty::Normal => crate::aim::DEFAULT_MAX_TURN,
            Difficulty::Hard => 40.0,
            Difficulty::Unfair => 100.0,
        }
    }

    /// Standard deviation of the aim error applied to every shot, in degrees.
    ///
    /// **Chosen.** For scale: a player-sized box is about 32 units wide, so at
    /// 500 units it subtends roughly 3.7 degrees. `Easy` at 6 degrees therefore
    /// misses a mid-range target more often than not; `Unfair` at 0.2 does not
    /// miss.
    pub fn aim_error_degrees(self) -> f64 {
        match self {
            Difficulty::Easy => 6.0,
            Difficulty::Normal => 2.5,
            Difficulty::Hard => 1.0,
            Difficulty::Unfair => 0.2,
        }
    }

    /// Seconds between an enemy becoming visible and the bot being willing to
    /// shoot at them.
    ///
    /// **Chosen.** Human visual reaction time is around 250 ms and a good
    /// player's is well under that once they are already expecting contact, so
    /// the ladder straddles it. `Unfair`'s 50 ms is deliberately inhuman.
    pub fn reaction_time(self) -> f32 {
        match self {
            Difficulty::Easy => 0.55,
            Difficulty::Normal => 0.30,
            Difficulty::Hard => 0.15,
            Difficulty::Unfair => 0.05,
        }
    }

    /// Rounds per burst for a [`crate::weapons::FireClass::FullAuto`] weapon.
    ///
    /// **Chosen**, but from a verified curve: spread is
    /// `shots³ / DIVISOR + 0.35` (`dlls/wpn_shared/wpn_ak47.cpp:97`), so the
    /// cube means shot 3 is nearly free and shot 8 is hopeless. A worse bot
    /// holds the trigger longer, which is both realistic and self-punishing.
    pub fn burst_shots(self) -> i32 {
        match self {
            Difficulty::Easy => 8,
            Difficulty::Normal => 5,
            Difficulty::Hard => 3,
            Difficulty::Unfair => 2,
        }
    }

    /// The fire-control parameters implied by this difficulty.
    pub fn fire_params(self) -> crate::fire::FireParams {
        crate::fire::FireParams {
            burst_shots: self.burst_shots(),
            ..crate::fire::FireParams::default()
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

    #[test]
    fn the_difficulty_ladder_is_coherent_in_every_axis() {
        // A bot that turns faster must also aim better, react quicker and
        // burst tighter — otherwise "harder" is not a difficulty, it is a
        // different bot.
        let ladder =
            [Difficulty::Easy, Difficulty::Normal, Difficulty::Hard, Difficulty::Unfair];
        for pair in ladder.windows(2) {
            let (lo, hi) = (pair[0], pair[1]);
            assert!(lo.max_turn() < hi.max_turn(), "{lo:?} -> {hi:?} turn");
            assert!(
                lo.aim_error_degrees() > hi.aim_error_degrees(),
                "{lo:?} -> {hi:?} aim error must shrink"
            );
            assert!(lo.reaction_time() > hi.reaction_time(), "{lo:?} -> {hi:?} reaction");
            assert!(lo.burst_shots() > hi.burst_shots(), "{lo:?} -> {hi:?} burst");
        }
    }

    #[test]
    fn every_difficulty_is_physically_plausible() {
        for d in [Difficulty::Easy, Difficulty::Normal, Difficulty::Hard, Difficulty::Unfair] {
            assert!(d.max_turn() > 0.0 && d.max_turn() <= 180.0, "{d:?}");
            assert!(d.aim_error_degrees() >= 0.0, "{d:?}");
            assert!(d.reaction_time() >= 0.0 && d.reaction_time() < 2.0, "{d:?}");
            assert!(d.burst_shots() >= 1, "{d:?} must be able to fire at all");
            assert_eq!(d.fire_params().burst_shots, d.burst_shots());
            assert!(
                d.fire_params().burst_resume_at < d.burst_shots(),
                "{d:?}: a burst that resumes at or above its cap never resumes"
            );
        }
    }
}
