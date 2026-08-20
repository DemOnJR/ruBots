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
//!
//! One value is a join of two sources rather than a copy of one:
//! `PlayerView::visible` is the map being clear **and** no living teammate
//! standing in the line. Players are not in the BSP, so the map trace alone
//! reports a clear shot straight through a teammate — see [`blocks_line`].

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

/// Worldspawn **plus** brush entities (`func_wall`, breakables, …).
///
/// Raw [`nav::bsp::Bsp`] only answers world hulls. Dust2 site geometry and
/// many crates live as separate models — without them bots "see" (and shoot)
/// through solid cover.
pub struct BrushSight<'a> {
    pub bsp: &'a nav::bsp::Bsp,
    pub brushes: &'a [nav::entities::SolidBrush],
}

impl Sight for BrushSight<'_> {
    fn visible(&self, from: [f32; 3], to: [f32; 3]) -> bool {
        use nav::bsp::Hull;
        if !self.bsp.visible(from, to) {
            return false;
        }
        for b in self.brushes {
            if b.model == 0 || b.model >= self.bsp.models.len() {
                continue;
            }
            let t = self.bsp.hull_trace_model(b.model, Hull::Point, from, to);
            if t.start_solid || t.fraction < 1.0 - 1e-4 {
                return false;
            }
        }
        true
    }
}

/// Half-width of a player's bounding box: `VEC_HULL_MIN/MAX` is
/// `(-16,-16,-36)..(16,16,36)` and `VEC_DUCK_HULL_MIN/MAX` is
/// `(-16,-16,-18)..(16,16,18)` (`regamedll/dlls/player.h`), so the footprint is
/// the same standing or ducking and only the height changes.
const PLAYER_HALF_WIDTH: f32 = 16.0;
const PLAYER_HALF_HEIGHT: f32 = 36.0;
const DUCK_HALF_HEIGHT: f32 = 18.0;

/// How much wider than the body to treat the line, because a burst is a cone
/// and not a ray.
///
/// The bot fires bursts while its own velocity and recoil are still moving the
/// muzzle, so "the centre line misses my teammate by an inch" is a teammate
/// who gets shot. This is the allowance for that, and it is deliberately
/// small: every unit of it is also a shot at an enemy that will not be taken.
const SPREAD_ALLOWANCE: f32 = 8.0;

/// Is `blocker` standing in the segment `from` → `to`?
///
/// Players are **not in the BSP**. `Bsp::visible` traces world geometry, so it
/// answers "is the map clear between these two points" and reports a clear
/// line straight through a teammate's body. For deciding whether to shoot,
/// that is the wrong question: a living player in the path stops the bullet
/// exactly as a wall does, and the fire control has no other gate — the whole
/// combat rung hangs off `visible_enemies()`
/// (`crates/bot/src/controller.rs:400-455`, `crates/bot/src/world.rs:262`).
///
/// A proper slab test: the segment against the blocker's axis-aligned box,
/// widened by [`SPREAD_ALLOWANCE`]. The overlap is clamped to `(0, 1)` so that
/// neither endpoint blocks itself.
///
/// It has to be a slab test and not "is the closest point on the segment to the
/// box's CENTRE inside the box". That shortcut is exact only for a sphere, and
/// a player is 24 units wide and 72 tall -- so a line entering a corner of the
/// box is nearest the centre at a point that has already left it. Measured by
/// brute force against a dense point sample, the closest-point version missed
/// **13% of true body-blocks**, always the grazing ones, which are exactly the
/// shots that clip a teammate's shoulder.
fn blocks_line(blocker: [f32; 3], ducking: bool, from: [f32; 3], to: [f32; 3]) -> bool {
    let d = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
    if d[0] * d[0] + d[1] * d[1] + d[2] * d[2] < 1.0 {
        return false;
    }
    let half_w = PLAYER_HALF_WIDTH + SPREAD_ALLOWANCE;
    let half = [
        half_w,
        half_w,
        if ducking {
            DUCK_HALF_HEIGHT
        } else {
            PLAYER_HALF_HEIGHT
        },
    ];

    // An endpoint inside the box means this "blocker" IS one of the two
    // parties, so it cannot be in the way of itself. Without this the shooter
    // is always inside its own box and can never fire at anything.
    let contains = |p: [f32; 3]| (0..3).all(|a| (p[a] - blocker[a]).abs() <= half[a]);
    if contains(from) || contains(to) {
        return false;
    }

    // Standard slab clipping: intersect the segment's parameter range with each
    // axis's slab in turn. If anything survives strictly inside (0, 1), the
    // segment passes through the box.
    let (mut t0, mut t1) = (0.0f32, 1.0f32);
    for axis in 0..3 {
        let (lo, hi) = (
            blocker[axis] - half[axis] - from[axis],
            blocker[axis] + half[axis] - from[axis],
        );
        if d[axis].abs() < 1e-6 {
            // Parallel to this slab: either always inside it or never.
            if lo > 0.0 || hi < 0.0 {
                return false;
            }
            continue;
        }
        let (mut near, mut far) = (lo / d[axis], hi / d[axis]);
        if near > far {
            std::mem::swap(&mut near, &mut far);
        }
        t0 = t0.max(near);
        t1 = t1.min(far);
        if t0 >= t1 {
            return false;
        }
    }
    t1 > 0.0 && t0 < 1.0
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
    speed: f32,
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
                Angles {
                    pitch: p[0],
                    yaw: p[1],
                }
            })
            .unwrap_or_default(),
        weapon: weapon_state(d),
        speed,
    };

    let seen = d.players();
    let my_team = g.my_team();
    // Living teammates, as bodies that stop bullets. Collected first because
    // the answer for any one player depends on where all the others are
    // standing. `Spectator`/`Unassigned` are not bodies on either side: a
    // spectator has no collision, and a team we are unsure of must not be
    // allowed to veto shots, or one unknown slot silently disarms the bot.
    let friendly_bodies: Vec<(u16, [f32; 3], bool)> = if my_team.is_playing() {
        seen.iter()
            .filter(|p| p.team == my_team && !g.player(p.entity as u8).is_some_and(|i| i.dead))
            .map(|p| (p.entity, p.origin, p.ducking))
            .collect()
    } else {
        Vec::new()
    };

    let players = seen
        .iter()
        .map(|p| {
            let target_eye = bot::math::eye_position(p.origin);
            // Two separate obstructions, and both have to be clear: the map
            // (from the BSP) and our own team (from the entity frame, because
            // players are not in the BSP at all).
            let world_clear = sight.is_some_and(|s| s.visible(eye, target_eye));
            let friendly_clear = !friendly_bodies
                .iter()
                .any(|&(e, o, duck)| e != p.entity && blocks_line(o, duck, eye, target_eye));
            PlayerView {
                entity: p.entity,
                origin: p.origin,
                // Not observable on a default server: entity_state_player_t has
                // no health field, and HealthInfo is gated on
                // scoreboard_showhealth, which reveals teammates only by
                // default. Full health is the conservative assumption -- it
                // never makes the bot over-commit.
                health: 100.0,
                team: team(p.team),
                alive: !g.player(p.entity as u8).is_some_and(|i| i.dead),
                visible: world_clear && friendly_clear,
                angles: Some(Angles {
                    pitch: p.angles[0],
                    yaw: p.angles[1],
                }),
            }
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

        cd.fields
            .insert("health".into(), proto::delta::Value::Float(100.0));
        d.clientdata = Some(cd);
        assert!(alive(&d), "full health with no deadflag is alive");
    }

    // --- friendly bodies in the line ---------------------------------------

    #[test]
    fn a_teammate_standing_in_the_line_blocks_it() {
        let from = [0.0, 0.0, 17.0];
        let to = [400.0, 0.0, 17.0];
        // Dead centre, halfway.
        assert!(blocks_line([200.0, 0.0, 0.0], false, from, to));
        // Off to the side by more than the hull plus the spread allowance.
        assert!(!blocks_line([200.0, 40.0, 0.0], false, from, to));
        // Just inside the hull.
        assert!(blocks_line([200.0, 20.0, 0.0], false, from, to));
        // Behind the shooter, and beyond the target: neither is in the way.
        assert!(!blocks_line([-100.0, 0.0, 0.0], false, from, to));
        assert!(!blocks_line([500.0, 0.0, 0.0], false, from, to));
    }

    /// A ducking teammate is half as tall, so a shot over their head is a shot
    /// that lands. Treating them as standing would silently cancel it.
    #[test]
    fn a_ducking_teammate_only_blocks_the_lower_line() {
        // A line 30 units above the blocker's origin.
        let from = [0.0, 0.0, 30.0];
        let to = [400.0, 0.0, 30.0];
        assert!(
            blocks_line([200.0, 0.0, 0.0], false, from, to),
            "standing is 36 tall"
        );
        assert!(
            !blocks_line([200.0, 0.0, 0.0], true, from, to),
            "ducked is 18 tall"
        );
    }

    #[test]
    fn a_degenerate_segment_blocks_nothing() {
        assert!(!blocks_line(
            [0.0, 0.0, 0.0],
            false,
            [1.0, 1.0, 1.0],
            [1.0, 1.0, 1.0]
        ));
    }

    /// The point of the gate: an enemy behind a teammate is not a shot.
    ///
    /// The map trace says the line is clear, because players are not in the
    /// BSP -- so without this the bot fires and the teammate takes it.
    #[test]
    fn an_enemy_behind_a_teammate_is_not_visible() {
        use crate::usermsg::Team as MT;

        const SIGNON: &[u8] = include_bytes!("../tests/fixtures/signon.bin");
        let s = crate::signon::walk(SIGNON);
        let mut d = Decoder::new(&s, crate::stream::UserMsgTable::default());

        // We are entity 1 and a Terrorist; entity 2 is a teammate 200 units
        // ahead, entity 3 an enemy 400 units ahead, directly behind them.
        d.my_slot = 0;
        d.game.self_index = Some(1);
        d.game.max_clients = 12;
        d.game.apply("TeamInfo", &team_info(1, "TERRORIST"));
        d.game.apply("TeamInfo", &team_info(2, "TERRORIST"));
        d.game.apply("TeamInfo", &team_info(3, "CT"));
        let mut cd = crate::world::ClientData::default();
        cd.fields
            .insert("health".into(), proto::delta::Value::Float(100.0));
        d.clientdata = Some(cd);
        d.entities = vec![
            player_entity(2, [200.0, 0.0, 0.0]),
            player_entity(3, [400.0, 0.0, 0.0]),
        ];

        let w = project(&d, Vec::new(), Some(&AllVisible), 0.0, 0.0);
        let enemy = w.players.iter().find(|p| p.entity == 3).expect("enemy 3");
        assert!(
            !enemy.visible,
            "the map is clear but a living teammate is standing in the line"
        );
        assert_eq!(
            w.visible_enemies().count(),
            0,
            "so there is nothing to shoot"
        );

        // Step the teammate aside and the shot is back on.
        d.entities[0] = player_entity(2, [200.0, 80.0, 0.0]);
        let w = project(&d, Vec::new(), Some(&AllVisible), 0.0, 0.0);
        assert!(w.players.iter().find(|p| p.entity == 3).unwrap().visible);
        assert_eq!(w.visible_enemies().count(), 1);

        // A *dead* teammate is not a body: corpses do not stop bullets.
        d.entities[0] = player_entity(2, [200.0, 0.0, 0.0]);
        d.game
            .apply("ScoreAttrib", &[2u8, crate::usermsg::ScoreAttrib::DEAD]);
        let w = project(&d, Vec::new(), Some(&AllVisible), 0.0, 0.0);
        assert!(
            w.players.iter().find(|p| p.entity == 3).unwrap().visible,
            "a corpse must not veto the shot"
        );
    }

    /// An *enemy* in the line is not a reason to hold fire -- shooting through
    /// one enemy to reach another is fine, and gating on it would make a bot
    /// facing two enemies shoot at neither.
    #[test]
    fn an_enemy_in_the_line_does_not_block() {
        const SIGNON: &[u8] = include_bytes!("../tests/fixtures/signon.bin");
        let s = crate::signon::walk(SIGNON);
        let mut d = Decoder::new(&s, crate::stream::UserMsgTable::default());
        d.my_slot = 0;
        d.game.self_index = Some(1);
        d.game.max_clients = 12;
        d.game.apply("TeamInfo", &team_info(1, "TERRORIST"));
        d.game.apply("TeamInfo", &team_info(2, "CT"));
        d.game.apply("TeamInfo", &team_info(3, "CT"));
        let mut cd = crate::world::ClientData::default();
        cd.fields
            .insert("health".into(), proto::delta::Value::Float(100.0));
        d.clientdata = Some(cd);
        d.entities = vec![
            player_entity(2, [200.0, 0.0, 0.0]),
            player_entity(3, [400.0, 0.0, 0.0]),
        ];
        let w = project(&d, Vec::new(), Some(&AllVisible), 0.0, 0.0);
        assert_eq!(w.visible_enemies().count(), 2);
    }

    fn team_info(client: u8, name: &str) -> Vec<u8> {
        let mut p = vec![client];
        p.extend_from_slice(name.as_bytes());
        p.push(0);
        p
    }

    /// An entity block the way the delta stream hands one over: only the
    /// fields the projection reads need to be present.
    fn player_entity(number: u16, origin: [f32; 3]) -> proto::entity::EntityState {
        let mut e = proto::entity::EntityState::default();
        e.number = number;
        for (k, v) in [
            ("origin[0]", origin[0]),
            ("origin[1]", origin[1]),
            ("origin[2]", origin[2]),
        ] {
            e.fields.insert(k.into(), proto::delta::Value::Float(v));
        }
        e
    }

    /// The slab test must agree with brute force, including on grazes.
    ///
    /// The version this replaced took the closest point on the segment to the
    /// box's CENTRE and tested that one point -- exact for a sphere, wrong for
    /// a 24x24x72 box. This walks each segment densely and asks whether ANY
    /// point on it is inside the box, which is the definition, and requires the
    /// analytic answer to match.
    #[test]
    fn the_body_block_test_agrees_with_brute_force_on_grazing_lines() {
        let blocker = [0.0f32, 0.0, 0.0];
        let half_w = PLAYER_HALF_WIDTH + SPREAD_ALLOWANCE;
        let half_h = PLAYER_HALF_HEIGHT;

        let inside = |p: [f32; 3]| {
            (p[0] - blocker[0]).abs() <= half_w
                && (p[1] - blocker[1]).abs() <= half_w
                && (p[2] - blocker[2]).abs() <= half_h
        };

        // A deterministic sweep of shooter/target pairs around the blocker,
        // deliberately dense near the box edges where grazes live.
        let mut checked = 0;
        let mut blocking = 0;
        for i in -14i32..=14 {
            for j in -14i32..=14 {
                let from = [-200.0, f32::from(i as i16) * 4.0, f32::from(j as i16) * 6.0];
                for k in -14i32..=14 {
                    let to = [200.0, f32::from(k as i16) * 4.0, f32::from(j as i16) * 6.0];

                    // Brute force: does any point of the segment enter the box?
                    let steps = 4000;
                    let mut truth = false;
                    for s in 1..steps {
                        let t = s as f32 / steps as f32;
                        let p = [
                            from[0] + (to[0] - from[0]) * t,
                            from[1] + (to[1] - from[1]) * t,
                            from[2] + (to[2] - from[2]) * t,
                        ];
                        if inside(p) {
                            truth = true;
                            break;
                        }
                    }
                    let got = blocks_line(blocker, false, from, to);
                    checked += 1;
                    if truth {
                        blocking += 1;
                    }
                    assert_eq!(
                        got, truth,
                        "from {from:?} to {to:?}: slab said {got}, brute force said {truth}"
                    );
                }
            }
        }
        assert!(checked > 10_000, "only checked {checked} lines");
        assert!(
            blocking > 100,
            "only {blocking} of {checked} lines actually blocked"
        );
    }

    /// Neither endpoint may block itself, or a bot can never shoot at all.
    #[test]
    fn the_shooter_and_the_target_do_not_block_their_own_line() {
        let from = [0.0, 0.0, 0.0];
        let to = [500.0, 0.0, 0.0];
        assert!(
            !blocks_line(from, false, from, to),
            "the shooter blocked itself"
        );
        assert!(
            !blocks_line(to, false, from, to),
            "the target blocked itself"
        );
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
