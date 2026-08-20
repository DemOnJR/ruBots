//! Planting and defusing, driven by what the server says rather than by a
//! private stopwatch.
//!
//! The distinction matters. A machine that starts a 3-second timer and declares
//! the bomb planted when it expires is wrong every time the plant was cancelled
//! — walked out of the zone, knocked off the ground, killed — and it is wrong
//! silently. These machines hold the button and wait to be *told*.
//!
//! The timers below are still here, but only to answer "have I been holding
//! this long enough that something should have happened by now", which is our
//! own bookkeeping about our own button, not a model of the server's state.

use crate::math::{distance, dot, forward, normalize, sub, Angles, Vec3};
use crate::weapons::WeaponId;
use crate::world::{Team, WorldView};

/// `C4_ARMING_ON_TIME` (`dlls/weapons.h:860`).
pub const C4_ARMING_ON_TIME: f32 = 3.0;

/// `MAX_PLAYER_USE_RADIUS` (`dlls/player.h:63`).
///
/// `CBasePlayer::PlayerUse` searches with
/// `UTIL_FindEntityInSphere(pObject, pev->origin, MAX_PLAYER_USE_RADIUS)`
/// (`dlls/player.cpp:4366`) — note the sphere is centred on **`pev->origin`**,
/// the feet, not the eyes.
pub const USE_RADIUS: f32 = 64.0;

/// `VIEW_FIELD_NARROW` (`dlls/util.h:42`) — `0.7`, commented "+-45 degrees".
///
/// The dot is taken between the player's forward and the vector **from the eyes**
/// to the object: `vecLOS = VecBModelOrigin(pObject->pev) - (pev->origin + pev->view_ofs)`
/// (`dlls/player.cpp:4374`). Feet for the range, eyes for the angle.
pub const VIEW_FIELD_NARROW: f32 = 0.7;

/// `NEXT_DEFUSE_TIME` (`dlls/ggrenade.cpp:1023`).
///
/// Every `+use` tick pushes `m_fNextDefuse = gpGlobals->time + NEXT_DEFUSE_TIME`
/// (`:1250`), and `CGrenade::C4Think` aborts the defuse the moment
/// `gpGlobals->time > m_fNextDefuse` (`:1573-1576`). So a gap longer than this
/// throws away all progress.
pub const NEXT_DEFUSE_TIME: f32 = 0.5;

/// Defuse time with a kit (`dlls/ggrenade.cpp:1052`).
pub const DEFUSE_TIME_KIT: f32 = 5.0;
/// Defuse time without one (`dlls/ggrenade.cpp:1067`).
pub const DEFUSE_TIME_NO_KIT: f32 = 10.0;

/// How long the bot holds `+use` before it gives up on a defuse that should
/// have finished. **Chosen** — a margin over the real duration, not a
/// completion condition.
pub const DEFUSE_PATIENCE: f32 = 3.0;

/// True when `target` is inside the use radius and inside the use cone.
///
/// Both halves as `CBasePlayer::PlayerUse` computes them: range from the feet
/// (`dlls/player.cpp:4366`), angle from the eyes against `pev->v_angle`
/// (`:4321`, `:4374-4377`).
pub fn can_use(origin: Vec3, view: Angles, target: Vec3, radius: f32, cone_cos: f32) -> bool {
    if distance(origin, target) > radius {
        return false;
    }
    let los = normalize(sub(target, crate::math::eye_position(origin)));
    if los == [0.0; 3] {
        return false;
    }
    dot(forward(view), los) > cone_cos
}

// ---------------------------------------------------------------------------
// Plant
// ---------------------------------------------------------------------------

/// Where a plant attempt is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlantPhase {
    /// Not trying.
    Idle,
    /// Holding something other than the C4; a `weapon_c4` has been asked for.
    Selecting,
    /// Holding `IN_ATTACK` with the C4 out, in the zone, on the ground.
    Arming,
    /// The server reported the bomb planted.
    Planted,
    /// The attempt was thrown away; see [`PlantAbort`].
    Aborted(PlantAbort),
}

/// Why a plant attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlantAbort {
    /// Left the bomb zone. `dlls/wpn_shared/wpn_c4.cpp:255-278` — the arming
    /// branch requires `onGround && inBombZone` every frame and cancels
    /// otherwise, printing `#C4_Arming_Cancelled`.
    LeftZone,
    /// Left the ground. Same branch (`#C4_Plant_Must_Be_On_Ground`).
    Airborne,
    /// The C4 is no longer the active weapon.
    LostWeapon,
    /// Died, or stopped being a terrorist.
    CannotPlant,
    /// Held the button long enough that the server should have confirmed, and
    /// it did not. Something is wrong; stop and let the ladder re-decide.
    TimedOutWithoutConfirmation,
}

/// The plant state machine.
///
/// One rule dominates everything: **`IN_ATTACK` must not be released for a
/// single tick** between starting and finishing. `CC4::WeaponIdle` runs the
/// moment the button is up and does `m_bStartedArming = false` plus a 1-second
/// `m_flNextPrimaryAttack` penalty (`dlls/wpn_shared/wpn_c4.cpp:287-300`), so
/// one dropped tick costs the whole three seconds *and* a second on top.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlantMachine {
    pub phase: PlantPhase,
    /// Seconds `IN_ATTACK` has been held unbroken.
    pub held: f32,
    /// Ticks in which the button was released while arming. Should be zero.
    pub released_ticks: u32,
}

impl Default for PlantMachine {
    fn default() -> Self {
        Self { phase: PlantPhase::Idle, held: 0.0, released_ticks: 0 }
    }
}

/// What the plant machine wants this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlantOutput {
    /// Hold `IN_ATTACK`.
    pub attack: bool,
    /// Switch to this weapon first.
    pub select: Option<WeaponId>,
}

impl PlantMachine {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    fn abort(&mut self, why: PlantAbort) -> PlantOutput {
        self.phase = PlantPhase::Aborted(why);
        self.held = 0.0;
        PlantOutput::default()
    }

    /// True while the machine is committed and must not be interrupted.
    pub fn is_arming(&self) -> bool {
        matches!(self.phase, PlantPhase::Arming)
    }

    pub fn is_done(&self) -> bool {
        matches!(self.phase, PlantPhase::Planted)
    }

    /// Advance one tick.
    ///
    /// The caller has already walked the bot to the site; this only decides
    /// the button. Preconditions are re-checked every tick because the server
    /// re-checks them every frame.
    pub fn tick(&mut self, world: &WorldView, dt: f32) -> PlantOutput {
        let me = &world.me;

        // The bomb being planted is the only confirmation that counts.
        if world.bomb.planted {
            self.phase = PlantPhase::Planted;
            self.held = 0.0;
            return PlantOutput::default();
        }

        if !me.alive || me.team != Team::Terrorist || !world.bomb.carried_by_me {
            let was_going = !matches!(self.phase, PlantPhase::Idle);
            self.held = 0.0;
            if was_going {
                return self.abort(PlantAbort::CannotPlant);
            }
            self.phase = PlantPhase::Idle;
            return PlantOutput::default();
        }

        // `bPlaceBomb = (onGround && inBombZone)` — wpn_c4.cpp:122.
        if !me.in_bomb_zone {
            self.held = 0.0;
            return if self.is_arming() {
                self.abort(PlantAbort::LeftZone)
            } else {
                self.phase = PlantPhase::Idle;
                PlantOutput::default()
            };
        }
        if !me.on_ground {
            self.held = 0.0;
            return if self.is_arming() {
                self.abort(PlantAbort::Airborne)
            } else {
                self.phase = PlantPhase::Idle;
                PlantOutput::default()
            };
        }

        // The C4 has to be the active weapon before IN_ATTACK means "arm".
        let holding_c4 = me.weapon_or_unknown().id == WeaponId::C4;
        if !holding_c4 {
            if self.is_arming() {
                self.held = 0.0;
                return self.abort(PlantAbort::LostWeapon);
            }
            self.phase = PlantPhase::Selecting;
            self.held = 0.0;
            // Explicitly *not* pressing attack: doing so with a gun out shoots
            // the floor next to the bomb site.
            return PlantOutput { attack: false, select: Some(WeaponId::C4) };
        }

        // Deploying the C4 sets `m_flNextAttack = 0.75` (`DefaultDeploy`,
        // `dlls/weapons.cpp:1509`) and `CBasePlayer::ItemPostFrame` returns
        // early for as long as that is in the future (`dlls/player.cpp:7422`).
        // So the first three quarters of a second of held `IN_ATTACK` after the
        // switch are simply not seen: `CC4::PrimaryAttack` is never reached and
        // no arming starts.
        //
        // Hold the button through it -- releasing is what cancels an arm, and
        // there is nothing to cancel yet -- but do not start a clock the server
        // has not started. Counting from the press instead would put our timer
        // 0.75 s ahead of the server's for the whole plant, which turns the
        // give-up deadline into a race we can lose for reasons that have
        // nothing to do with the plant.
        //
        // The weapon's own countdown answers this exactly rather than by
        // modelling it: `m_flNextPrimaryAttack` rides in `weapon_data_t`, and
        // `<= 0` means the server will act on the button now.
        if !me.weapon_or_unknown().primary_ready() {
            self.phase = PlantPhase::Selecting;
            self.held = 0.0;
            return PlantOutput { attack: true, select: None };
        }

        if !self.is_arming() {
            self.phase = PlantPhase::Arming;
            self.held = 0.0;
            self.released_ticks = 0;
        }

        self.held += dt;

        // Confirmation is `bomb.planted`, handled at the top. If we have held
        // well past the arming time and nothing came back, stop rather than
        // hold the button forever.
        if self.held > C4_ARMING_ON_TIME + DEFUSE_PATIENCE {
            return self.abort(PlantAbort::TimedOutWithoutConfirmation);
        }

        PlantOutput { attack: true, select: None }
    }

    /// Record that the button was actually released this tick — for the caller
    /// to report when something upstream overrode the plant. Purely diagnostic.
    pub fn note_released(&mut self) {
        if self.is_arming() {
            self.released_ticks += 1;
            self.held = 0.0;
        }
    }
}

// ---------------------------------------------------------------------------
// Defuse
// ---------------------------------------------------------------------------

/// Where a defuse attempt is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefusePhase {
    Idle,
    /// Not yet in range or not yet looking at it.
    Approaching,
    /// Holding `IN_USE` on the bomb.
    Defusing,
    Defused,
    Aborted(DefuseAbort),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefuseAbort {
    /// Drifted outside `MAX_PLAYER_USE_RADIUS`, or looked away.
    OutOfReach,
    /// Left the ground — `dlls/ggrenade.cpp:1571-1575` ends the defuse.
    Airborne,
    /// Not a CT, or dead. `CGrenade::Use` returns immediately for non-CTs
    /// (`dlls/ggrenade.cpp:1240-1243`).
    NotEligible,
    /// Held long past the expected duration with no result.
    TimedOutWithoutConfirmation,
}

/// The defuse state machine.
///
/// `+use` on the C4 is `FCAP_CONTINUOUS_USE`, so it is a *hold*, not a press:
/// `if ((pev->button & IN_USE) && (caps & FCAP_CONTINUOUS_USE))`
/// (`dlls/player.cpp:4413`). No rising edge is needed and re-pressing gains
/// nothing.
///
/// The hard constraint is the **gap**. Each `+use` tick renews
/// `m_fNextDefuse = time + 0.5` (`dlls/ggrenade.cpp:1250`), and `C4Think`
/// cancels as soon as `gpGlobals->time > m_fNextDefuse` (`:1573`). Half a
/// second of not pressing and the whole defuse is gone — with a full 10 seconds
/// to redo and, on a live bomb, no time to redo it in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DefuseMachine {
    pub phase: DefusePhase,
    /// Seconds spent holding `+use`.
    pub held: f32,
    /// Seconds since `+use` was last asserted. Must never exceed
    /// [`NEXT_DEFUSE_TIME`] once started.
    pub gap: f32,
    /// The largest gap seen during this attempt. Diagnostic.
    pub worst_gap: f32,
}

impl Default for DefuseMachine {
    fn default() -> Self {
        Self { phase: DefusePhase::Idle, held: 0.0, gap: 0.0, worst_gap: 0.0 }
    }
}

/// What the defuse machine wants this tick.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DefuseOutput {
    /// Hold `IN_USE`.
    pub use_action: bool,
    /// Aim here — being inside the cone is a precondition, so the machine owns
    /// the aim while it is running.
    pub look_at: Option<Vec3>,
    /// Move toward here; `None` means stand still.
    pub move_to: Option<Vec3>,
}

impl DefuseMachine {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn is_defusing(&self) -> bool {
        matches!(self.phase, DefusePhase::Defusing)
    }

    pub fn is_done(&self) -> bool {
        matches!(self.phase, DefusePhase::Defused)
    }

    /// Expected duration for this bot (`dlls/ggrenade.cpp:1052` / `:1067`).
    pub fn expected_duration(has_kit: bool) -> f32 {
        if has_kit {
            DEFUSE_TIME_KIT
        } else {
            DEFUSE_TIME_NO_KIT
        }
    }

    fn abort(&mut self, why: DefuseAbort) -> DefuseOutput {
        self.phase = DefusePhase::Aborted(why);
        self.held = 0.0;
        self.gap = 0.0;
        DefuseOutput::default()
    }

    /// Advance one tick. `view` is where the bot is currently looking.
    pub fn tick(&mut self, world: &WorldView, view: Angles, dt: f32) -> DefuseOutput {
        let me = &world.me;

        if world.bomb.defused || !world.bomb.planted {
            // Gone, one way or another. `!planted` after having been planted is
            // either a defuse or a detonation; either way there is nothing to
            // do and pretending to know which would be a guess.
            if self.is_defusing() && world.bomb.defused {
                self.phase = DefusePhase::Defused;
            } else if !self.is_done() {
                self.phase = DefusePhase::Idle;
            }
            self.held = 0.0;
            self.gap = 0.0;
            return DefuseOutput::default();
        }

        if !me.alive || me.team != Team::CounterTerrorist {
            return if self.is_defusing() {
                self.abort(DefuseAbort::NotEligible)
            } else {
                self.phase = DefusePhase::Idle;
                DefuseOutput::default()
            };
        }

        let Some(bomb) = world.bomb.origin else {
            self.phase = DefusePhase::Idle;
            return DefuseOutput::default();
        };

        // `CGrenade::Use` refuses to *start* off the ground
        // (dlls/ggrenade.cpp:1255-1260) and `C4Think` cancels an ongoing
        // defuse the moment `!iOnGround` (`:1571-1575`).
        if !me.on_ground {
            return if self.is_defusing() {
                self.abort(DefuseAbort::Airborne)
            } else {
                self.phase = DefusePhase::Approaching;
                DefuseOutput { look_at: Some(bomb), move_to: Some(bomb), ..Default::default() }
            };
        }

        let in_reach = can_use(me.origin, view, bomb, USE_RADIUS, VIEW_FIELD_NARROW);

        if !in_reach {
            if self.is_defusing() {
                // Every tick out of reach is a tick of gap, and half a second
                // of it throws the defuse away.
                self.gap += dt;
                self.worst_gap = self.worst_gap.max(self.gap);
                if self.gap >= NEXT_DEFUSE_TIME {
                    return self.abort(DefuseAbort::OutOfReach);
                }
            } else {
                self.phase = DefusePhase::Approaching;
            }
            // Close the distance and get the bomb into the cone.
            let too_far = distance(me.origin, bomb) > USE_RADIUS * 0.75;
            return DefuseOutput {
                use_action: false,
                look_at: Some(bomb),
                move_to: if too_far { Some(bomb) } else { None },
            };
        }

        if !self.is_defusing() {
            self.phase = DefusePhase::Defusing;
            self.held = 0.0;
            self.gap = 0.0;
            self.worst_gap = 0.0;
        }
        self.held += dt;
        self.gap = 0.0;

        if self.held > Self::expected_duration(me.has_defuse_kit) + DEFUSE_PATIENCE {
            return self.abort(DefuseAbort::TimedOutWithoutConfirmation);
        }

        // Stand still: any movement risks leaving the 64-unit sphere, and the
        // server freezes the defuser anyway (`SET_CLIENT_MAXSPEED(edict, 1)`,
        // `dlls/ggrenade.cpp:1030`).
        DefuseOutput { use_action: true, look_at: Some(bomb), move_to: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fire::WeaponState;
    use crate::math::aim_angles;
    use crate::world::{BombState, SelfState};

    const SITE: Vec3 = [1000.0, 1000.0, 0.0];

    fn carrier_at(at: Vec3, in_zone: bool, weapon: WeaponId) -> WorldView {
        WorldView {
            me: SelfState {
                origin: at,
                team: Team::Terrorist,
                in_bomb_zone: in_zone,
                on_ground: true,
                weapon: Some(WeaponState { id: weapon, ..Default::default() }),
                ..Default::default()
            },
            bomb: BombState { carried_by_me: true, ..Default::default() },
            ..Default::default()
        }
    }

    #[test]
    fn the_plant_selects_the_c4_before_it_presses_anything() {
        let mut m = PlantMachine::default();
        let w = carrier_at(SITE, true, WeaponId::Ak47);
        let out = m.tick(&w, 0.05);
        assert_eq!(out.select, Some(WeaponId::C4));
        assert!(!out.attack, "pressing attack with a rifle out just shoots the floor");
        assert_eq!(m.phase, PlantPhase::Selecting);
    }

    #[test]
    fn the_plant_holds_attack_for_three_seconds_with_no_released_ticks() {
        let mut m = PlantMachine::default();
        let w = carrier_at(SITE, true, WeaponId::C4);
        let dt = 0.05;
        let ticks = (C4_ARMING_ON_TIME / dt).ceil() as i32;

        let mut released = 0;
        for tick in 0..=ticks {
            let out = m.tick(&w, dt);
            if !out.attack {
                released += 1;
                eprintln!("released at tick {tick}");
            }
        }
        assert_eq!(released, 0, "one released tick cancels the whole plant");
        assert!(m.held >= C4_ARMING_ON_TIME, "held only {}", m.held);
        assert_eq!(m.phase, PlantPhase::Arming);
        assert_eq!(m.released_ticks, 0);
    }

    /// Switching to the C4 costs 0.75 s of `m_flNextAttack` before
    /// `ItemPostFrame` will look at the button at all. The bot must hold the
    /// button through that window -- letting go is what cancels an arm -- but
    /// must not credit itself for time the server never counted.
    #[test]
    fn the_arming_clock_starts_when_the_server_will_accept_the_button() {
        let mut m = PlantMachine::default();
        let mut w = carrier_at(SITE, true, WeaponId::C4);
        // Freshly deployed: the countdown is still running.
        w.me.weapon.as_mut().unwrap().next_primary_attack = 0.75;

        for _ in 0..15 {
            let out = m.tick(&w, 0.05);
            assert!(out.attack, "the button has to stay down across the deploy delay");
        }
        assert_eq!(m.held, 0.0, "counted {} s the server had not started", m.held);
        assert_eq!(m.phase, PlantPhase::Selecting);

        // The countdown expires; now the clock is real.
        w.me.weapon.as_mut().unwrap().next_primary_attack = 0.0;
        m.tick(&w, 0.05);
        assert_eq!(m.phase, PlantPhase::Arming);
        assert!((m.held - 0.05).abs() < 1e-6, "held {}", m.held);
    }

    #[test]
    fn the_plant_aborts_the_moment_the_zone_flag_clears() {
        let mut m = PlantMachine::default();
        let inside = carrier_at(SITE, true, WeaponId::C4);
        for _ in 0..20 {
            assert!(m.tick(&inside, 0.05).attack);
        }
        let outside = carrier_at(SITE, false, WeaponId::C4);
        let out = m.tick(&outside, 0.05);
        assert!(!out.attack, "must let go once out of the zone");
        assert_eq!(m.phase, PlantPhase::Aborted(PlantAbort::LeftZone));
        assert_eq!(m.held, 0.0, "progress is gone, not paused");
    }

    #[test]
    fn the_plant_aborts_when_knocked_off_the_ground() {
        let mut m = PlantMachine::default();
        let w = carrier_at(SITE, true, WeaponId::C4);
        m.tick(&w, 0.05);
        let mut airborne = w;
        airborne.me.on_ground = false;
        assert!(!m.tick(&airborne, 0.05).attack);
        assert_eq!(m.phase, PlantPhase::Aborted(PlantAbort::Airborne));
    }

    #[test]
    fn the_plant_is_only_confirmed_by_the_server() {
        // Holding for four seconds proves nothing on its own.
        let mut m = PlantMachine::default();
        let w = carrier_at(SITE, true, WeaponId::C4);
        for _ in 0..80 {
            m.tick(&w, 0.05);
        }
        assert!(m.held >= C4_ARMING_ON_TIME, "held {}", m.held);
        assert_ne!(m.phase, PlantPhase::Planted, "a private timer is not confirmation");

        let mut planted = w;
        planted.bomb.planted = true;
        let out = m.tick(&planted, 0.05);
        assert_eq!(m.phase, PlantPhase::Planted);
        assert!(!out.attack, "let go once it is done");
    }

    #[test]
    fn the_plant_gives_up_rather_than_holding_the_button_forever() {
        let mut m = PlantMachine::default();
        let w = carrier_at(SITE, true, WeaponId::C4);
        let mut aborted = false;
        for _ in 0..1000 {
            if !m.tick(&w, 0.05).attack {
                aborted = true;
                break;
            }
        }
        assert!(aborted, "should stop eventually");
        assert_eq!(m.phase, PlantPhase::Aborted(PlantAbort::TimedOutWithoutConfirmation));
    }

    #[test]
    fn a_non_carrier_never_presses_anything() {
        let mut m = PlantMachine::default();
        let mut w = carrier_at(SITE, true, WeaponId::C4);
        w.bomb.carried_by_me = false;
        let out = m.tick(&w, 0.05);
        assert!(!out.attack && out.select.is_none());
        assert_eq!(m.phase, PlantPhase::Idle);
    }

    // -- defuse --------------------------------------------------------------

    fn ct_at(at: Vec3, kit: bool) -> WorldView {
        WorldView {
            me: SelfState {
                origin: at,
                team: Team::CounterTerrorist,
                has_defuse_kit: kit,
                on_ground: true,
                ..Default::default()
            },
            bomb: BombState { planted: true, origin: Some(SITE), ..Default::default() },
            ..Default::default()
        }
    }

    fn looking_at(from: Vec3, target: Vec3) -> Angles {
        aim_angles(from, target)
    }

    #[test]
    fn the_defuse_never_lets_the_use_gap_exceed_half_a_second() {
        // The bot is nudged in and out of reach; whatever happens, it must
        // either keep +use asserted or give up — never drift over 0.5 s of gap
        // while still believing it is defusing.
        let mut m = DefuseMachine::default();
        let near: Vec3 = [SITE[0] + 30.0, SITE[1], 0.0];
        let far: Vec3 = [SITE[0] + 400.0, SITE[1], 0.0];
        let dt = 0.05;

        let mut gap = 0.0f32;
        let mut worst = 0.0f32;
        let mut started = false;

        for tick in 0..400 {
            // Wobble out of reach for a couple of ticks now and then.
            let out_of_reach = tick % 37 == 0 || tick % 37 == 1;
            let at = if out_of_reach { far } else { near };
            let w = ct_at(at, true);
            let view = looking_at(at, SITE);
            let out = m.tick(&w, view, dt);

            if out.use_action {
                started = true;
                gap = 0.0;
            } else if started && m.is_defusing() {
                gap += dt;
                worst = worst.max(gap);
                assert!(
                    gap < NEXT_DEFUSE_TIME,
                    "gap {gap} at tick {tick} would have cancelled the defuse"
                );
            } else {
                gap = 0.0;
            }
        }
        assert!(started, "it should have started defusing at some point");
        assert!(worst < NEXT_DEFUSE_TIME, "worst observed gap {worst}");
    }

    #[test]
    fn a_long_interruption_aborts_rather_than_pretending_to_continue() {
        let mut m = DefuseMachine::default();
        let near: Vec3 = [SITE[0] + 30.0, SITE[1], 0.0];
        let far: Vec3 = [SITE[0] + 400.0, SITE[1], 0.0];

        for _ in 0..10 {
            let w = ct_at(near, true);
            assert!(m.tick(&w, looking_at(near, SITE), 0.05).use_action);
        }
        assert!(m.is_defusing());

        // Ten ticks of 50 ms out of reach is 0.5 s — the cancel threshold.
        for _ in 0..10 {
            let w = ct_at(far, true);
            m.tick(&w, looking_at(far, SITE), 0.05);
        }
        assert_eq!(m.phase, DefusePhase::Aborted(DefuseAbort::OutOfReach));
        assert!(!m.is_defusing(), "it must not claim to still be defusing");
    }

    #[test]
    fn the_defuse_needs_both_the_range_and_the_cone() {
        let at: Vec3 = [SITE[0] + 30.0, SITE[1], 0.0];
        let w = ct_at(at, true);

        // In range, looking at it.
        let mut m = DefuseMachine::default();
        assert!(m.tick(&w, looking_at(at, SITE), 0.05).use_action);

        // In range, looking the other way — VIEW_FIELD_NARROW rejects it.
        let mut m = DefuseMachine::default();
        let away = Angles { pitch: 0.0, yaw: looking_at(at, SITE).yaw + 90.0 };
        assert!(!m.tick(&w, away, 0.05).use_action, "outside the +-45 degree cone");

        // Looking at it, but 100 units away — outside MAX_PLAYER_USE_RADIUS.
        let mut m = DefuseMachine::default();
        let far: Vec3 = [SITE[0] + 100.0, SITE[1], 0.0];
        let w = ct_at(far, true);
        assert!(!m.tick(&w, looking_at(far, SITE), 0.05).use_action, "outside 64 units");
    }

    #[test]
    fn the_use_radius_is_measured_from_the_feet_and_the_cone_from_the_eyes() {
        // Straight down at a bomb 60 units below: inside the sphere from the
        // feet, and the cone has to be computed from the eyes or the angle is
        // wrong by the view height.
        let origin: Vec3 = [0.0, 0.0, 60.0];
        let bomb: Vec3 = [0.0, 0.0, 0.0];
        assert!((distance(origin, bomb) - 60.0).abs() < 1e-3);
        let view = aim_angles(origin, bomb);
        assert!(can_use(origin, view, bomb, USE_RADIUS, VIEW_FIELD_NARROW));
        // 65 units away is outside.
        let origin: Vec3 = [0.0, 0.0, 65.0];
        assert!(!can_use(origin, aim_angles(origin, bomb), bomb, USE_RADIUS, VIEW_FIELD_NARROW));
    }

    #[test]
    fn a_terrorist_does_not_defuse() {
        let at: Vec3 = [SITE[0] + 30.0, SITE[1], 0.0];
        let mut w = ct_at(at, false);
        w.me.team = Team::Terrorist;
        let mut m = DefuseMachine::default();
        assert!(!m.tick(&w, looking_at(at, SITE), 0.05).use_action);
        assert_eq!(m.phase, DefusePhase::Idle);
    }

    #[test]
    fn leaving_the_ground_aborts_the_defuse() {
        let at: Vec3 = [SITE[0] + 30.0, SITE[1], 0.0];
        let w = ct_at(at, true);
        let mut m = DefuseMachine::default();
        assert!(m.tick(&w, looking_at(at, SITE), 0.05).use_action);
        let mut airborne = w;
        airborne.me.on_ground = false;
        assert!(!m.tick(&airborne, looking_at(at, SITE), 0.05).use_action);
        assert_eq!(m.phase, DefusePhase::Aborted(DefuseAbort::Airborne));
    }

    #[test]
    fn the_kit_halves_the_expected_duration() {
        assert_eq!(DefuseMachine::expected_duration(true), 5.0);
        assert_eq!(DefuseMachine::expected_duration(false), 10.0);
    }

    #[test]
    fn a_defused_bomb_ends_the_machine_cleanly() {
        let at: Vec3 = [SITE[0] + 30.0, SITE[1], 0.0];
        let w = ct_at(at, true);
        let mut m = DefuseMachine::default();
        m.tick(&w, looking_at(at, SITE), 0.05);
        let mut done = w;
        done.bomb.defused = true;
        let out = m.tick(&done, looking_at(at, SITE), 0.05);
        assert!(!out.use_action);
        assert_eq!(m.phase, DefusePhase::Defused);
        assert!(m.is_done());
    }
}
