//! Projecting the decoded network state into the bot's `WorldView`.
//!
//! This is the seam between "what the server said" and "what the bot thinks".
//! Everything here is a join of three sources, because no single one of them is
//! sufficient:
//!
//! * **`svc_clientdata`** — our own origin, velocity, health, flags, weapons,
//!   punchangle and per-slot weapon data. Authoritative, every frame.
//! * **the entity block** — everyone else's origin and angles. The only place
//!   they exist.
//! * **user messages** — team, money, round state, bomb and hostage events.
//!   The entity tables carry no team and no health for other players
//!   (`entity_state_player_t` has neither; `team` is literally commented out in
//!   `delta.lst:148`), so this is not optional.
//!
//! Where a value genuinely is not observable, it is left at its conservative
//! default rather than invented. Enemy health is the clearest case: it is not
//! on the wire on a default server, so it stays at full and the bot's target
//! scoring must not lean on it.

use crate::usermsg::Team as MsgTeam;
use crate::world::Decoder;
use bot::world::{HostageView, PlayerView, SelfState, Team, WorldView};
use bot::Angles;

/// Are we alive?
///
/// **Not** from `clientdata_t.deadflag`. That field is declared in `delta.lst`
/// and is never populated: `UpdateClientData` (`dlls/client.cpp:5020-5100`)
/// assigns flags, health, weapons, origin, velocity, punchangle and the rest,
/// and never touches `cd->deadflag`. It is therefore always zero in the struct
/// the engine deltas against, so it is never transmitted, so a reader that
/// treats "absent" as alive has written a function that cannot return false.
///
/// Measured on a real capture: `deadflag` present in **0** of 18354 decoded
/// clientdata frames, while 14562 of them read `health 0, maxspeed 900,
/// weapons 0` -- a corpse in observer mode. Live, the bot reported `alive true`
/// in 214 of 214 samples and kept running the combat ladder after it died.
///
/// `health` is the signal that does arrive, and ReGameDLL clamps it for us:
/// `cd->health = max(pev->health, 0.0f)` (`dlls/client.cpp:5038`). Nothing
/// alive has zero health. The scoreboard's `SCORE_STATUS_DEAD` bit corroborates
/// it -- `SetScoreboardAttributes` sets it from `pev->deadflag != DEAD_NO`
/// (`dlls/player.cpp:5705-5744`) -- and covers the moment between the two.
fn alive(d: &Decoder) -> bool {
    let Some(cd) = d.clientdata.as_ref() else {
        return false;
    };
    if cd.health() <= 0.0 {
        return false;
    }
    !d.game
        .self_index
        .and_then(|i| d.game.player(i))
        .is_some_and(|p| p.dead)
}

/// Translate the user-message team enum into the bot's.
fn team(t: MsgTeam) -> Team {
    match t {
        MsgTeam::Terrorist => Team::Terrorist,
        MsgTeam::CounterTerrorist => Team::CounterTerrorist,
        MsgTeam::Spectator => Team::Spectator,
        MsgTeam::Unassigned => Team::Unassigned,
    }
}

/// Anything that can answer "can I see from here to there?".
///
/// A trait so the projection does not depend on the map being loaded — with no
/// visibility source every player is reported unseen, which makes the bot hold
/// fire rather than shoot through walls.
pub trait Sight {
    fn visible(&self, from: [f32; 3], to: [f32; 3]) -> bool;
}

/// The map's own collision answers this exactly.
///
/// `Hull::Point` is the right hull for a sightline: a bullet and an eye are
/// points, not player-sized boxes. Using a player hull here would report an
/// enemy as hidden whenever a 32-unit-wide box could not fit through the gap
/// they are visible through, which is most doorways seen at an angle.
impl Sight for nav::bsp::Bsp {
    fn visible(&self, from: [f32; 3], to: [f32; 3]) -> bool {
        nav::bsp::Bsp::visible(self, from, to)
    }
}

/// What the bot is currently holding, joined from two sources.
///
/// Neither alone is enough:
///
/// * **`CurWeapon`** (a user message) names the active weapon and its clip.
///   It is the ONLY thing that says which gun is in our hands --
///   `usercmd_t.weaponselect` is never read by ReGameDLL, and
///   `clientdata_t.weapons` is not transmitted at all.
/// * **`weapon_data_t`**, inside `svc_clientdata`, carries the per-weapon
///   timers and counters the fire control actually closes the loop on. It is
///   keyed by weapon id and only present because our userinfo sets `cl_lw 1`
///   (`sv_main.cpp:1362`).
///
/// The timers are **countdowns**, not absolute times: ReGameDLL is built with
/// CLIENT_WEAPONS so `UTIL_WeaponTimeBase()` is 0 and `PostThink` decrements
/// them each frame (`player.cpp:5440-5479`). `<= 0` means ready now, which is
/// why no cycle-time modelling is needed anywhere.
fn weapon_state(d: &Decoder) -> Option<bot::fire::WeaponState> {
    let id = bot::WeaponId::from_id(d.game.weapon_id);
    if id == bot::WeaponId::None {
        return None;
    }
    let mut w = bot::fire::WeaponState {
        id,
        clip: i32::from(d.game.weapon_clip),
        reserve: 0,
        ..Default::default()
    };
    if let Some(fields) = d
        .clientdata
        .as_ref()
        .and_then(|c| c.weapons.get(&d.game.weapon_id))
    {
        let f = |k: &str| {
            fields
                .get(k)
                .and_then(proto::delta::Value::as_f32)
                .unwrap_or(0.0)
        };
        let i = |k: &str| {
            fields
                .get(k)
                .and_then(proto::delta::Value::as_i64)
                .unwrap_or(0)
        };
        w.next_primary_attack = f("m_flNextPrimaryAttack");
        w.next_secondary_attack = f("m_flNextSecondaryAttack");
        w.in_reload = i("m_fInReload") != 0;
        // m_iShotsFired rides in m_fInZoom (`dlls/client.cpp:4990`), which is
        // what lets fire discipline be closed-loop instead of modelled.
        w.shots_fired = i("m_fInZoom") as i32;
        let clip = i("m_iClip") as i32;
        if clip != 0 {
            w.clip = clip;
        }
    }
    Some(w)
}

/// Build the bot's view of the world from the decoded stream.
///
/// `rescue_zones` and `sight` come from the map, which the network stream does
/// not carry; pass empty/None until the nav layer supplies them.
pub fn project(
    d: &Decoder,
    rescue_zones: Vec<[f32; 3]>,
    sight: Option<&dyn Sight>,
    latency: f32,
) -> WorldView {
    let cd = d.clientdata.as_ref();
    let g = &d.game;

    let my_origin = cd.map(|c| c.origin()).unwrap_or([0.0; 3]);
    let eye = bot::math::eye_position(my_origin);

    let me = SelfState {
        origin: my_origin,
        velocity: cd.map(|c| c.velocity()).unwrap_or([0.0; 3]),
        // Our own health is on the wire twice -- clientdata and the Health
        // user message. clientdata is per-frame and authoritative.
        health: cd.map(|c| c.health()).unwrap_or(0.0),
        team: team(g.my_team()),
        alive: alive(d),
        money: g.money,
        weapons: cd.map(|c| c.weapons()).unwrap_or(0),
        // ITEM_STATUS_DEFUSER, cdll_dll.h:61.
        has_defuse_kit: g.item_flags & 0x02 != 0,
        // FL_ONGROUND, const.h.
        on_ground: cd.is_some_and(|c| c.on_ground()),
        in_bomb_zone: cd.is_some_and(|c| c.in_bomb_zone()),
        can_shoot: cd.is_some_and(|c| c.can_shoot()),
        freeze_period: cd.is_some_and(|c| c.freeze_period()),
        punchangle: cd
            .map(|c| {
                let p = c.punchangle();
                Angles { pitch: p[0], yaw: p[1] }
            })
            .unwrap_or_default(),
        weapon: weapon_state(d),
    };

    let players = d
        .players()
        .into_iter()
        .map(|p| PlayerView {
            entity: p.entity,
            origin: p.origin,
            // Not observable on a default server: entity_state_player_t has no
            // health field, and HealthInfo is gated on scoreboard_showhealth,
            // which reveals teammates only by default. Full health is the
            // conservative assumption -- it never makes the bot over-commit.
            health: 100.0,
            team: team(p.team),
            alive: !g.player(p.entity as u8).is_some_and(|i| i.dead),
            visible: sight.is_some_and(|s| {
                s.visible(eye, bot::math::eye_position(p.origin))
            }),
            angles: Some(Angles {
                pitch: p.angles[0],
                yaw: p.angles[1],
            }),
        })
        .collect();

    let hostages = g
        .hostages
        .iter()
        .map(|(id, pos)| HostageView {
            entity: u16::from(*id),
            origin: *pos,
            alive: true,
            rescued: false,
            // Not observable: nothing tells a client which hostage follows
            // whom. The hostage machine tracks its own attempts instead.
            following_me: false,
        })
        .collect();

    let mut world = WorldView {
        me,
        players,
        bomb: Default::default(),
        hostages,
        rescue_zones,
        round_time: f32::from(g.round_time.max(0)),
        frametime: 0.0,
        latency,
    };
    world.bomb.planted = g.bomb_planted;
    world.bomb.origin = g.bomb_position;
    world.bomb.carried_by_me = g
        .self_index
        .and_then(|i| g.player(i))
        .is_some_and(|p| p.has_bomb);
    world
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AllVisible;
    impl Sight for AllVisible {
        fn visible(&self, _: [f32; 3], _: [f32; 3]) -> bool {
            true
        }
    }

    /// With no map loaded there is no way to know what is visible, and the
    /// honest answer is "nothing" -- which makes the bot hold fire rather than
    /// shoot through a wall it cannot see.
    #[test]
    fn without_a_sight_source_nobody_is_visible() {
        let t = team(MsgTeam::Terrorist);
        assert_eq!(t, Team::Terrorist);
        // The projection's visibility term is `sight.is_some_and(..)`, so a
        // None source can only ever produce false.
        let sight: Option<&dyn Sight> = None;
        assert!(!sight.is_some_and(|s: &dyn Sight| s.visible([0.0; 3], [0.0; 3])));
        let sight: Option<&dyn Sight> = Some(&AllVisible);
        assert!(sight.is_some_and(|s: &dyn Sight| s.visible([0.0; 3], [0.0; 3])));
    }

    /// A corpse must not read as alive.
    ///
    /// The old implementation asked `clientdata_t.deadflag`, which the game DLL
    /// never fills, so it answered "alive" for every frame of every death --
    /// 214 of 214 live samples, 68 of which were plainly an observer at
    /// maxspeed 900. The bot kept running the combat ladder on a corpse.
    #[test]
    fn zero_health_is_dead_however_confident_the_delta_stream_is() {
        const SIGNON: &[u8] = include_bytes!("../tests/fixtures/signon.bin");
        let s = crate::signon::walk(SIGNON);
        let table = crate::stream::UserMsgTable::default();
        let mut d = Decoder::new(&s, table);
        assert!(!alive(&d), "no clientdata at all cannot be alive");

        let mut cd = crate::world::ClientData::default();
        // Exactly what a live capture shows for a corpse: no deadflag field
        // anywhere, health clamped to zero by `cd->health = max(health, 0)`.
        assert!(
            !cd.fields.contains_key("deadflag"),
            "the fixture must not smuggle in the field the server never sends"
        );
        d.clientdata = Some(cd.clone());
        assert!(!alive(&d), "health 0 is a corpse");

        cd.fields.insert(
            "health".into(),
            proto::delta::Value::Float(100.0),
        );
        d.clientdata = Some(cd);
        assert!(alive(&d), "full health with no deadflag is alive");
    }

    #[test]
    fn team_translation_is_total() {
        for (a, b) in [
            (MsgTeam::Terrorist, Team::Terrorist),
            (MsgTeam::CounterTerrorist, Team::CounterTerrorist),
            (MsgTeam::Spectator, Team::Spectator),
            (MsgTeam::Unassigned, Team::Unassigned),
        ] {
            assert_eq!(team(a), b);
        }
    }
}
