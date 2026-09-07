//! The bot's view of the game.
//!
//! The AI never touches the netcode. The client layer decodes
//! `entity_state_t` / `clientdata_t` / `weapon_data_t` deltas and fills this
//! snapshot in; the AI reads it and returns commands. That keeps every decision
//! testable without a server attached.
//!
//! Field names mirror the delta fields the original reads in `Think`
//! (`origin[0..2]`, `velocity[0..2]`, `health`, `flags`, `weapons`) and
//! `findTarget` (`view_ofs[2]`, `number`).
//!
//! ## What is observable, and what is not
//!
//! This distinction decides what the AI is allowed to be clever about, so it is
//! spelled out per field below. The three that catch people:
//!
//! * **Other players' health is not observable.** `entity_state_t` has no
//!   health field for other players on a default server; only the local
//!   player's own health arrives, in `clientdata_t`. Anything that scores
//!   targets by how hurt they are is scoring a constant.
//! * **Rescue zones are not observable.** `func_hostage_rescue` sets
//!   `SIGNAL_RESCUE` (`dlls/triggers.cpp:1991`), but the only signal that
//!   reaches `iuser3` is `SIGNAL_BOMB` (`dlls/client.cpp:5107-5109`). The
//!   zones have to come from the map, so [`WorldView::rescue_zones`] is an
//!   input, not something the bot can discover.
//! * **`PLAYER_FREEZE_TIME_OVER` is misnamed.** It is set *during* the freeze
//!   period — `if (g_pGameRules->IsFreezePeriod()) iUser3 |= PLAYER_FREEZE_TIME_OVER`
//!   (`dlls/client.cpp:5104-5105`) — so the field here is called
//!   [`SelfState::freeze_period`], which is what it means.

use crate::fire::WeaponState;
use crate::math::{eye_position, forward, normalize, sub, Angles, Vec3};

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
    /// **Not observable on a default server** — see the module docs. Left in
    /// the struct because a modded server or a spectator feed can supply it,
    /// and because the type would otherwise have to change if one does. Treat
    /// a snapshot where every enemy reads 100 as "unknown", not as "healthy".
    pub health: f32,
    pub team: Team,
    pub alive: bool,
    /// Whether the bot can actually see them. Filled by the BSP trace; until
    /// that lands the client can conservatively report `false`.
    pub visible: bool,
    /// Their view angles, from the `angles` delta field. `None` when the
    /// snapshot did not carry them.
    pub angles: Option<Angles>,
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
            angles: None,
        }
    }
}

impl PlayerView {
    /// How squarely this player is facing `point`, as a cosine in `[-1, 1]`.
    ///
    /// `None` when their angles were not in the snapshot — the honest answer,
    /// rather than a default that would read as "facing away".
    pub fn facing_cosine(&self, point: Vec3) -> Option<f32> {
        let angles = self.angles?;
        let to_point = normalize(sub(point, eye_position(self.origin)));
        if to_point == [0.0; 3] {
            return None;
        }
        Some(crate::math::dot(forward(angles), to_point))
    }

    /// True when this player is pointing within `cone_cos` of `point`.
    ///
    /// Unknown angles answer `false`: an enemy that might be aiming at us is
    /// not evidence that they are.
    pub fn is_aiming_at(&self, point: Vec3, cone_cos: f32) -> bool {
        self.facing_cosine(point).is_some_and(|c| c > cone_cos)
    }
}

/// A hostage, as far as a CT can tell.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HostageView {
    pub entity: u16,
    pub origin: Vec3,
    pub alive: bool,
    /// True when this hostage is following *this* bot.
    ///
    /// Not directly on the wire — the caller infers it from having successfully
    /// used the hostage and from it tracking the bot's position. Report `false`
    /// when unsure; the escort machine treats it as "not picked up yet", which
    /// is the safe direction.
    pub following_me: bool,
    /// True once the hostage has been delivered.
    pub rescued: bool,
}

impl Default for HostageView {
    fn default() -> Self {
        Self {
            entity: 0,
            origin: [0.0; 3],
            alive: true,
            following_me: false,
            rescued: false,
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
    /// True once the round's bomb has been defused.
    pub defused: bool,
}

impl Default for BombState {
    fn default() -> Self {
        Self { planted: false, origin: None, carried_by_me: false, defused: false }
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
    /// `pev->flags & FL_ONGROUND`. Both the plant
    /// (`dlls/wpn_shared/wpn_c4.cpp:111`) and the defuse
    /// (`dlls/ggrenade.cpp:1255-1260`, and the abort at `:1571-1575`) require
    /// it, so this is not cosmetic.
    pub on_ground: bool,
    /// `clientdata.iuser3 & PLAYER_IN_BOMB_ZONE` — `BIT(2)`,
    /// `dlls/cdll_dll.h:72`, set from `SIGNAL_BOMB` at `dlls/client.cpp:5107`.
    pub in_bomb_zone: bool,
    /// `iuser3 & PLAYER_CAN_SHOOT` (`BIT(0)`). Cleared while defusing
    /// (`dlls/client.cpp:5099-5101`).
    pub can_shoot: bool,
    /// `iuser3 & PLAYER_FREEZE_TIME_OVER` (`BIT(1)`), which despite the name is
    /// set **during** the freeze period — see the module docs.
    pub freeze_period: bool,
    /// `clientdata.punchangle`. Applied to the shot direction twice; see
    /// [`crate::aim::compensate`].
    pub punchangle: Angles,
    /// The active weapon as `weapon_data_t` last described it.
    pub weapon: Option<WeaponState>,
    /// Horizontal speed, **measured** rather than reported.
    ///
    /// `clientdata_t` has a velocity field and this server does not send it: a
    /// client with prediction on computes its own, so the bits are saved.
    /// Trusting the absent field reads zero, which would tell a bot sprinting
    /// across the map that it is standing still -- and standing still is
    /// exactly the precondition every weapon's accuracy depends on.
    pub speed: f32,
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
            on_ground: true,
            in_bomb_zone: false,
            can_shoot: true,
            freeze_period: false,
            punchangle: Angles::default(),
            weapon: None,
            // Standing still by default, so a test that does not care about
            // movement gets the accurate branch of every weapon rather than a
            // silent "too fast to shoot".
            speed: 0.0,
        }
    }
}

impl SelfState {
    /// The weapon state, or a benign default when nothing has arrived yet.
    ///
    /// The default is [`crate::weapons::WeaponId::None`], whose fire class is
    /// `NotAWeapon` — so "we do not know what we are holding" resolves to "do
    /// not shoot", not to "shoot anyway".
    pub fn weapon_or_unknown(&self) -> WeaponState {
        self.weapon.unwrap_or_default()
    }

    pub fn eye(&self) -> Vec3 {
        eye_position(self.origin)
    }
}

/// Something the bot heard: a gunshot, a footstep, a door.
///
/// The engine only sends a sound to clients inside its PAS, so anything that
/// reaches us is something this player could genuinely hear — the audibility
/// question is already answered by the time it gets here. What remains is how
/// loud it was at our ears and how long ago, which is what decides whether it
/// is worth turning to look at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Heard {
    pub origin: Vec3,
    /// 0 (inaudible) to 1 (right on top of us).
    pub loudness: f32,
    /// Seconds since it happened.
    pub age: f32,
}

impl Heard {
    /// Loudness discounted by how stale it is. Zero once it stops mattering.
    pub fn urgency(&self) -> f32 {
        const FADE: f32 = 3.0;
        if self.age >= FADE {
            return 0.0;
        }
        self.loudness * (1.0 - self.age / FADE)
    }
}

/// Everything the AI is allowed to look at.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorldView {
    pub me: SelfState,
    pub players: Vec<PlayerView>,
    pub bomb: BombState,
    pub hostages: Vec<HostageView>,
    /// Centres of the map's `func_hostage_rescue` brushes.
    ///
    /// **An input, not an observation** — see the module docs. Empty means the
    /// bot does not know where to take a hostage, and it will escort rather
    /// than wander.
    pub rescue_zones: Vec<Vec3>,
    /// Seconds since the round started.
    pub round_time: f32,
    /// The server's frame length, for forward-predicting punchangle decay.
    /// Defaults to 0 (no prediction), which is the conservative choice.
    pub frametime: f32,
    /// Round-trip latency in seconds — how stale everything in here is.
    pub latency: f32,
    /// Sounds heard recently, oldest first. Our own noises are already
    /// filtered out: a bot must not turn to look at its own footsteps.
    pub sounds: Vec<Heard>,
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

    /// The sound most worth turning toward, if any.
    ///
    /// Loudness decays with age rather than being cut off: a shot half a
    /// second ago outranks a footstep now, and a two-second-old footstep is
    /// still a place worth checking, just not urgently.
    pub fn loudest_sound(&self) -> Option<&Heard> {
        self.sounds
            .iter()
            .max_by(|a, b| a.urgency().total_cmp(&b.urgency()))
            .filter(|h| h.urgency() > 0.0)
    }

    /// Living, un-rescued hostages.
    pub fn live_hostages(&self) -> impl Iterator<Item = &HostageView> {
        self.hostages.iter().filter(|h| h.alive && !h.rescued)
    }

    /// Hostages currently following this bot.
    pub fn my_hostages(&self) -> impl Iterator<Item = &HostageView> {
        self.hostages.iter().filter(|h| h.alive && !h.rescued && h.following_me)
    }

    /// How many engine frames of punchangle decay to predict forward.
    ///
    /// The observed punch is one round trip old, so at least that much decay
    /// has already happened that we cannot see. Rounds down and floors at zero:
    /// under-predicting leaves a little over-compensation, which pulls shots
    /// *down*, and low is a much better miss than high.
    pub fn punch_prediction_frames(&self) -> u32 {
        if self.frametime <= 0.0 || self.latency <= 0.0 {
            return 0;
        }
        (self.latency / self.frametime) as u32
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

    #[test]
    fn an_enemy_facing_us_is_recognised_as_such() {
        // Enemy 200 units down +x, looking back along -x at us.
        let me: Vec3 = [0.0, 0.0, 0.0];
        let p = PlayerView {
            origin: [200.0, 0.0, 0.0],
            angles: Some(Angles { pitch: 0.0, yaw: 180.0 }),
            ..Default::default()
        };
        let cos = p.facing_cosine(crate::math::eye_position(me)).unwrap();
        assert!(cos > 0.99, "should be facing straight at us, cos {cos}");
        assert!(p.is_aiming_at(crate::math::eye_position(me), 0.7));

        // Turned 90 degrees away.
        let side = PlayerView { angles: Some(Angles { pitch: 0.0, yaw: 90.0 }), ..p };
        assert!(!side.is_aiming_at(crate::math::eye_position(me), 0.7));
    }

    #[test]
    fn unknown_angles_never_read_as_aiming_at_us() {
        let p = PlayerView { origin: [200.0, 0.0, 0.0], angles: None, ..Default::default() };
        assert_eq!(p.facing_cosine([0.0; 3]), None);
        assert!(!p.is_aiming_at([0.0; 3], 0.7), "unknown must not mean threatening");
    }

    #[test]
    fn an_unknown_weapon_resolves_to_something_that_will_not_be_fired() {
        use crate::weapons::{fire_class, FireClass, WeaponId};
        let s = SelfState::default();
        assert_eq!(s.weapon_or_unknown().id, WeaponId::None);
        assert_eq!(fire_class(s.weapon_or_unknown().id), FireClass::NotAWeapon);
    }

    #[test]
    fn hostage_filters_skip_the_dead_and_the_delivered() {
        let w = WorldView {
            hostages: vec![
                HostageView { entity: 1, following_me: true, ..Default::default() },
                HostageView { entity: 2, alive: false, ..Default::default() },
                HostageView { entity: 3, rescued: true, following_me: true, ..Default::default() },
                HostageView { entity: 4, ..Default::default() },
            ],
            ..Default::default()
        };
        assert_eq!(w.live_hostages().map(|h| h.entity).collect::<Vec<_>>(), vec![1, 4]);
        assert_eq!(w.my_hostages().map(|h| h.entity).collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn punch_prediction_is_zero_when_we_cannot_know() {
        let mut w = WorldView::default();
        assert_eq!(w.punch_prediction_frames(), 0, "no timing info, no prediction");
        w.frametime = 0.05;
        assert_eq!(w.punch_prediction_frames(), 0, "no latency, no prediction");
        w.latency = 0.1;
        assert_eq!(w.punch_prediction_frames(), 2);
        w.latency = 0.09; // rounds down rather than up
        assert_eq!(w.punch_prediction_frames(), 1);
    }
}
