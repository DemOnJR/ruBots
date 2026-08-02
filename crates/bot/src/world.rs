//! The bot's view of the game.
//!
//! The AI never touches the netcode. The client layer decodes
//! `entity_state_t` / `clientdata_t` deltas and fills this snapshot in; the AI
//! reads it and returns commands. That keeps every decision testable without a
//! server attached.
//!
//! Field names mirror the delta fields the original reads in `Think`
//! (`origin[0..2]`, `velocity[0..2]`, `health`, `flags`, `weapons`) and
//! `findTarget` (`view_ofs[2]`, `number`).

use crate::math::Vec3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Team {
    Terrorist,
    CounterTerrorist,
    Spectator,
    Unassigned,
}

impl Team {
    pub fn is_enemy_of(self, other: Team) -> bool {
        matches!(
            (self, other),
            (Team::Terrorist, Team::CounterTerrorist)
                | (Team::CounterTerrorist, Team::Terrorist)
        )
    }
}

/// Another player as the bot currently understands them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayerView {
    /// Entity index — the `number` delta field.
    pub entity: u16,
    pub origin: Vec3,
    pub health: f32,
    pub team: Team,
    pub alive: bool,
    /// Whether the bot can actually see them. Filled by the BSP trace; until
    /// that lands the client can conservatively report `false`.
    pub visible: bool,
}

impl Default for PlayerView {
    fn default() -> Self {
        Self {
            entity: 0,
            origin: [0.0; 3],
            health: 100.0,
            team: Team::Unassigned,
            alive: true,
            visible: false,
        }
    }
}

/// State of the objective.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BombState {
    pub planted: bool,
    /// Where the bomb is, once known.
    pub origin: Option<Vec3>,
    /// True while this bot is carrying it.
    pub carried_by_me: bool,
}

impl Default for BombState {
    fn default() -> Self {
        Self { planted: false, origin: None, carried_by_me: false }
    }
}

/// The bot's own state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SelfState {
    pub origin: Vec3,
    pub velocity: Vec3,
    pub health: f32,
    pub team: Team,
    pub alive: bool,
    pub money: i32,
    /// Bitmask from the `weapons` delta field.
    pub weapons: u32,
    pub has_defuse_kit: bool,
}

impl Default for SelfState {
    fn default() -> Self {
        Self {
            origin: [0.0; 3],
            velocity: [0.0; 3],
            health: 100.0,
            team: Team::Unassigned,
            alive: true,
            money: 800,
            weapons: 0,
            has_defuse_kit: false,
        }
    }
}

/// Everything the AI is allowed to look at.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorldView {
    pub me: SelfState,
    pub players: Vec<PlayerView>,
    pub bomb: BombState,
    /// Seconds since the round started.
    pub round_time: f32,
}

impl WorldView {
    /// Living, visible players on the opposing team.
    pub fn visible_enemies(&self) -> impl Iterator<Item = &PlayerView> {
        let my_team = self.me.team;
        self.players
            .iter()
            .filter(move |p| p.alive && p.visible && p.team.is_enemy_of(my_team))
    }

    /// Living enemies whether or not they can be seen.
    pub fn known_enemies(&self) -> impl Iterator<Item = &PlayerView> {
        let my_team = self.me.team;
        self.players
            .iter()
            .filter(move |p| p.alive && p.team.is_enemy_of(my_team))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn teams_are_enemies_only_across_the_divide() {
        assert!(Team::Terrorist.is_enemy_of(Team::CounterTerrorist));
        assert!(Team::CounterTerrorist.is_enemy_of(Team::Terrorist));
        assert!(!Team::Terrorist.is_enemy_of(Team::Terrorist));
        assert!(!Team::Terrorist.is_enemy_of(Team::Spectator));
        assert!(!Team::Spectator.is_enemy_of(Team::Terrorist));
    }

    fn world() -> WorldView {
        WorldView {
            me: SelfState { team: Team::Terrorist, ..Default::default() },
            players: vec![
                PlayerView {
                    entity: 1,
                    team: Team::CounterTerrorist,
                    visible: true,
                    ..Default::default()
                },
                PlayerView {
                    entity: 2,
                    team: Team::CounterTerrorist,
                    visible: false,
                    ..Default::default()
                },
                PlayerView {
                    entity: 3,
                    team: Team::CounterTerrorist,
                    visible: true,
                    alive: false,
                    ..Default::default()
                },
                PlayerView {
                    entity: 4,
                    team: Team::Terrorist,
                    visible: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn visible_enemies_excludes_teammates_dead_and_unseen() {
        let w = world();
        let ids: Vec<u16> = w.visible_enemies().map(|p| p.entity).collect();
        assert_eq!(ids, vec![1], "only the living, visible enemy");
    }

    #[test]
    fn known_enemies_includes_the_unseen_but_not_the_dead() {
        let w = world();
        let ids: Vec<u16> = w.known_enemies().map(|p| p.entity).collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn a_bot_with_no_team_has_no_enemies() {
        let mut w = world();
        w.me.team = Team::Unassigned;
        assert_eq!(w.visible_enemies().count(), 0);
    }
}
