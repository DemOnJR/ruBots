//! Aim smoothing — how the bot turns toward what it wants to shoot.
//!
//! Port of `(*Bot).turn` (`0x140702FE0`). This is the single most
//! behaviour-defining routine in the AI: it is what makes a bot look like a
//! player rather than an aimbot.
//!
//! Verified formula:
//!
//! ```text
//! step = clamp(norm_angle(desired - current) * 0.45, -max, +max)
//! ```
//!
//! * the `0.45` factor is at `0x141149140` (`mulsd xmm0, xmm2`)
//! * `max` is the bot's own turn speed when set, otherwise **20.0**
//!   (`0x141149170`); the negative bound is produced by `pxor` against `-0.0`
//! * the clamp is the `ucomisd`/`jbe` ladder at `0x140703072`
//! * pitch is held to +/-89 degrees (`0x141149268` / `0x1411491A8`)
//!
//! Both axes go through `normAngle` *before* scaling, so turning from 350 to
//! 10 degrees takes the short way round.

//! ## Punchangle (added here, verified against ReGameDLL/ReHLDS)
//!
//! Recoil is applied to the shot direction **twice**, and a bot that
//! compensates once will shoot high by exactly half the kick. The chain, in
//! the order one server frame executes it:
//!
//! 1. `PM_CheckParameters` decays the punch, then folds it into the view:
//!    `VectorAdd(v_angle, pmove->punchangle, v_angle)` and
//!    `pmove->angles[PITCH] = v_angle[PITCH]`
//!    (`regamedll/pm_shared/pm_shared.cpp:3035-3046`). `v_angle` there starts
//!    as `cmd.viewangles` — the angles *we* sent.
//! 2. `SV_RunCmd` copies that straight back onto the entity:
//!    `sv_player->v.v_angle[0] = pmove->angles[0]`
//!    (`rehlds/engine/sv_user.cpp:1011-1013`). So `pev->v_angle` is already
//!    `sent + punch`.
//! 3. The weapon then fires along
//!    `UTIL_MakeVectors(pev->v_angle + pev->punchangle)`
//!    (`dlls/wpn_shared/wpn_ak47.cpp:122`, and the same line in every other
//!    weapon; `dlls/weapons.cpp:687` for the shell ejection). That is
//!    `sent + 2 * punch`.
//!
//! So to put a bullet along `desired`, send `desired - 2 * punch`.
//!
//! The punch we hold is a round trip stale — it was sampled from a `clientdata`
//! update that has already crossed the wire once, and it decays every frame in
//! between — so [`predict_punch`] runs the engine's own decay forward before
//! [`compensate`] uses it. The decay is
//! `len -= (10.0 + len * 0.5) * frametime` applied to the *length* of the punch
//! vector, not per axis (`PM_DropPunchAngle`,
//! `regamedll/pm_shared/pm_shared.cpp:2990-3002`).

use crate::math::{norm_angle, Angles};

/// Fraction of the remaining angular error covered per tick.
pub const TURN_FACTOR: f64 = 0.45;

/// Default per-tick cap in degrees when a bot has no difficulty-specific rate.
pub const DEFAULT_MAX_TURN: f64 = 20.0;

/// The engine refuses pitch beyond this.
pub const PITCH_LIMIT: f32 = 89.0;

/// One smoothing step toward `desired`.
///
/// `max_turn` is the per-tick cap in degrees — lower is a "worse" bot.
/// Spring-damper gains for the view.
///
/// `turn_toward` is `step = err * 0.45`, clamped. That is monotone: it can never
/// overshoot, never rings, and decelerates along the same geometric curve
/// whatever the distance. A head does not move like that. A head is a mass on a
/// muscle -- it accelerates, it arrives with momentum, and on a fast swing it
/// goes slightly past and comes back.
///
/// Integrated with **explicit Euler on purpose**:
///
/// ```text
/// e      = norm_angle(desired - current)
/// accel  = clamp(k*e - c*v, -a_max, +a_max)
/// v     += dt * accel        // persists across ticks -- this is the momentum
/// angle += dt * v
/// ```
///
/// The acceleration clamp saturates above `e = a_max/k` (15 degrees for nav
/// yaw, 10 for combat), so small corrections are a pure spring and big swings
/// are bang-bang. That two-regime shape is why small corrections look precise
/// and big swings look thrown.
///
/// **Do not "improve" this with RK4 or a semi-implicit step.** At combat-pitch
/// gains `omega*dt` is about 0.82, and Euler's energy gain is a large part of
/// the overshoot. The numerical artifact is the feature.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpringGains {
    pub k: f64,
    pub c: f64,
    pub a_max: f64,
}

/// Walking around: stiff enough to be purposeful, damped enough not to ring.
pub const NAV_GAINS: SpringGains = SpringGains { k: 200.0, c: 25.0, a_max: 3000.0 };
/// Fighting: faster, and deliberately underdamped so a hard flick overshoots.
pub const COMBAT_GAINS: SpringGains = SpringGains { k: 300.0, c: 20.0, a_max: 3300.0 };

/// Longest tick the integrator will accept, in seconds.
///
/// A stall must not be integrated as one enormous step: `dt` of half a second
/// at these gains launches the view across the map. Clamping is the honest
/// response -- the head simply did not move during the hitch.
pub const MAX_DT: f64 = 1.0 / 25.0;

/// Yaw snaps and stops inside this; pitch never does.
///
/// The asymmetry is deliberate. A permanently-excited, lightly-damped pitch
/// axis -- driven by the eye height bobbing as the bot walks -- is what keeps
/// the crosshair alive. Zeroing it would restore exactly the dead-still view
/// this replaces: 42.7% of consecutive samples once shared an identical integer
/// yaw.
pub const YAW_DEADBAND: f64 = 1.0;

/// The view's angular velocity, carried between ticks. This IS the momentum.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ViewMotion {
    pub yaw_vel: f64,
    pub pitch_vel: f64,
}

impl ViewMotion {
    /// Advance the view one tick toward `desired`.
    ///
    /// Pitch uses `2k` with the same damping and clamp: the eye is quicker to
    /// drop and lift than to swing, and the extra stiffness at unchanged `c` is
    /// what leaves that axis under-damped enough to keep breathing.
    pub fn step(
        &mut self,
        current: Angles,
        desired: Angles,
        gains: SpringGains,
        dt: f64,
    ) -> Angles {
        self.step_with_yaw_error(current, desired, gains, dt, None)
    }

    /// One spring step with YaPB's back-swing guard
    /// (`yapb/src/vision.cpp:172-195`) applied at the navigation rungs.
    ///
    /// `travel_yaw` is the bearing of the point being walked to, in degrees.
    /// A head on a swivel passes behind itself on huge turns, wraps, and shows
    /// the spectator the back of the skull for half a second -- and it is
    /// exactly what a spring integrator does when current and desired straddle
    /// the direction of travel with the short way round pointing behind it.
    /// When that is the case the error is shifted by a full 360 (in the sign of
    /// its short way) so the integrator swings round the front instead: on a
    /// nav rung the head leads the body, it does not trail it.
    ///
    /// Condition and adjustment are YaPB's: with `c` and `t` the signed,
    /// travel-relative yaws of current and desired, force the long way when
    /// `c * t < 0` (they straddle the direction of travel) and
    /// `|c - t| >= 180` (so the short arc is the one that goes behind).
    pub fn step_guarded(
        &mut self,
        current: Angles,
        desired: Angles,
        gains: SpringGains,
        dt: f64,
        travel_yaw: f64,
    ) -> Angles {
        let ey = norm_angle(f64::from(desired.yaw) - f64::from(current.yaw));
        if travel_yaw.abs() > 1e-4 {
            let c = norm_angle(f64::from(current.yaw) - travel_yaw);
            let t = norm_angle(f64::from(desired.yaw) - travel_yaw);
            if c * t < 0.0 && (c - t).abs() >= 180.0 {
                // The short way round goes through the back: take the long way
                // through the front. The deadband then cannot snap the spring
                // into a wrap either, because the error is no longer small.
                let guarded = if ey > 0.0 { ey - 360.0 } else { ey + 360.0 };
                return self.step_with_yaw_error(current, desired, gains, dt, Some(guarded));
            }
        }
        self.step_with_yaw_error(current, desired, gains, dt, Some(ey))
    }

    /// The actual integration; `yaw_error` is None for the plain path and the
    /// possibly-guarded error otherwise.
    fn step_with_yaw_error(
        &mut self,
        current: Angles,
        desired: Angles,
        gains: SpringGains,
        dt: f64,
        yaw_error: Option<f64>,
    ) -> Angles {
        let dt = dt.clamp(1e-4, MAX_DT);

        let ey = yaw_error.unwrap_or_else(|| {
            norm_angle(f64::from(desired.yaw) - f64::from(current.yaw))
        });
        let yaw = if ey.abs() < YAW_DEADBAND {
            // Close enough: stop, and kill the momentum so it cannot ring here.
            self.yaw_vel = 0.0;
            f64::from(desired.yaw)
        } else {
            let a = (gains.k * ey - gains.c * self.yaw_vel).clamp(-gains.a_max, gains.a_max);
            self.yaw_vel += dt * a;
            f64::from(current.yaw) + dt * self.yaw_vel
        };

        let ep = norm_angle(f64::from(desired.pitch) - f64::from(current.pitch));
        let ap =
            (2.0 * gains.k * ep - gains.c * self.pitch_vel).clamp(-gains.a_max, gains.a_max);
        self.pitch_vel += dt * ap;
        let pitch = f64::from(current.pitch) + dt * self.pitch_vel;

        Angles {
            pitch: (norm_angle(pitch) as f32).clamp(-PITCH_LIMIT, PITCH_LIMIT),
            yaw: norm_angle(yaw) as f32,
        }
    }
}

pub fn turn_toward(current: Angles, desired: Angles, max_turn: f64) -> Angles {
    let max = max_turn.abs();

    let dyaw = norm_angle(f64::from(desired.yaw) - f64::from(current.yaw));
    let yaw_step = (dyaw * TURN_FACTOR).clamp(-max, max);

    let dpitch = norm_angle(f64::from(desired.pitch) - f64::from(current.pitch));
    let pitch_step = (dpitch * TURN_FACTOR).clamp(-max, max);

    let yaw = norm_angle(f64::from(current.yaw) + yaw_step) as f32;
    let pitch = (norm_angle(f64::from(current.pitch) + pitch_step) as f32)
        .clamp(-PITCH_LIMIT, PITCH_LIMIT);

    Angles { pitch, yaw }
}

/// How far off target the bot still is, in degrees.
///
/// The engagement logic uses this to decide whether it is worth pulling the
/// trigger yet.
pub fn aim_error(current: Angles, desired: Angles) -> f64 {
    let dy = norm_angle(f64::from(desired.yaw) - f64::from(current.yaw));
    let dp = norm_angle(f64::from(desired.pitch) - f64::from(current.pitch));
    (dy * dy + dp * dp).sqrt()
}

/// How many times the punchangle lands on the shot direction. See the module
/// docs: once via `pmove->angles` -> `pev->v_angle`, once in the weapon's own
/// `UTIL_MakeVectors`.
pub const PUNCH_APPLICATIONS: f64 = 2.0;

/// The constant term in `PM_DropPunchAngle` (`pm_shared.cpp:2995`).
pub const PUNCH_DECAY_BASE: f64 = 10.0;
/// The proportional term in `PM_DropPunchAngle` (`pm_shared.cpp:2995`).
pub const PUNCH_DECAY_RATE: f64 = 0.5;

/// One engine frame of punchangle decay, exactly as `PM_DropPunchAngle` does it.
///
/// The decay is on the **length** of the punch vector: normalise, shrink the
/// length, scale back. Doing it per axis would be wrong whenever the punch has
/// both a pitch and a yaw component, which it does as soon as `KickBack`'s
/// lateral term fires (`dlls/weapons.cpp:756-762`).
///
/// Roll (`punchangle[2]`) is not modelled: the only thing that writes it is the
/// landing thump (`pm_shared.cpp:2925`), which does not affect where bullets
/// go. Ignoring it makes the computed length slightly small, i.e. the decay
/// slightly slow, i.e. this over-compensates rather than under-compensates.
pub fn decay_punch_step(punch: Angles, frametime: f32) -> Angles {
    let p = f64::from(punch.pitch);
    let y = f64::from(punch.yaw);
    let len = (p * p + y * y).sqrt();
    if len <= f64::EPSILON {
        return Angles::default();
    }
    let shrunk =
        (len - (PUNCH_DECAY_BASE + len * PUNCH_DECAY_RATE) * f64::from(frametime)).max(0.0);
    let k = shrunk / len;
    Angles { pitch: (p * k) as f32, yaw: (y * k) as f32 }
}

/// Run the decay forward over `frames` engine frames of `frametime` each.
///
/// Iterating the exact single step rather than integrating the ODE, because the
/// engine's Euler step is what actually happens — over the one or two frames of
/// latency this covers, the difference is not academic at high kick values.
pub fn predict_punch(punch: Angles, frametime: f32, frames: u32) -> Angles {
    let mut p = punch;
    for _ in 0..frames {
        p = decay_punch_step(p, frametime);
    }
    p
}

/// Bend the angles we send so the *bullet* goes where we want.
///
/// `desired` is where the bullet should go; `punch` is the punchangle expected
/// to be in force at the moment the shot resolves (run it through
/// [`predict_punch`] first). Returns the angles to put in `usercmd_t`.
pub fn compensate(desired: Angles, punch: Angles) -> Angles {
    let pitch = norm_angle(f64::from(desired.pitch) - PUNCH_APPLICATIONS * f64::from(punch.pitch));
    let yaw = norm_angle(f64::from(desired.yaw) - PUNCH_APPLICATIONS * f64::from(punch.yaw));
    Angles {
        pitch: (pitch as f32).clamp(-PITCH_LIMIT, PITCH_LIMIT),
        yaw: yaw as f32,
    }
}

/// Where a bullet actually goes for a given sent angle and punch — the forward
/// model [`compensate`] inverts. Only used to prove the inverse in tests, but
/// it is the honest statement of what the server does.
pub fn resolve_shot(sent: Angles, punch: Angles) -> Angles {
    Angles {
        pitch: norm_angle(f64::from(sent.pitch) + PUNCH_APPLICATIONS * f64::from(punch.pitch))
            as f32,
        yaw: norm_angle(f64::from(sent.yaw) + PUNCH_APPLICATIONS * f64::from(punch.yaw)) as f32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ang(pitch: f32, yaw: f32) -> Angles {
        Angles { pitch, yaw }
    }

    #[test]
    fn turning_converges_on_the_target() {
        let desired = ang(0.0, 90.0);
        let mut cur = ang(0.0, 0.0);
        for _ in 0..200 {
            cur = turn_toward(cur, desired, DEFAULT_MAX_TURN);
        }
        assert!(
            aim_error(cur, desired) < 0.01,
            "should have settled, error {}",
            aim_error(cur, desired)
        );
    }

    #[test]
    fn a_single_step_never_exceeds_the_cap() {
        // 180 degrees of error, capped at 20 per tick.
        let before = ang(0.0, 0.0);
        let after = turn_toward(before, ang(0.0, 179.0), DEFAULT_MAX_TURN);
        let moved = norm_angle(f64::from(after.yaw) - f64::from(before.yaw)).abs();
        assert!(moved <= DEFAULT_MAX_TURN + 1e-9, "moved {moved} > cap");
        assert!(moved > 19.0, "should be saturated at the cap, moved {moved}");
    }

    #[test]
    fn small_errors_use_the_045_factor_not_the_cap() {
        let after = turn_toward(ang(0.0, 0.0), ang(0.0, 10.0), DEFAULT_MAX_TURN);
        // 10 * 0.45 = 4.5
        assert!((f64::from(after.yaw) - 4.5).abs() < 1e-6, "yaw {}", after.yaw);
    }

    #[test]
    fn turning_takes_the_short_way_around_the_wrap() {
        // From 350 to 10 is +20, not -340.
        let after = turn_toward(ang(0.0, 170.0), ang(0.0, -170.0), DEFAULT_MAX_TURN);
        let moved = norm_angle(f64::from(after.yaw) - 170.0);
        assert!(moved > 0.0, "should turn positively through 180, moved {moved}");
        assert!(moved <= DEFAULT_MAX_TURN + 1e-9);
    }

    #[test]
    fn pitch_is_clamped_to_the_engine_limit() {
        let mut cur = ang(0.0, 0.0);
        for _ in 0..200 {
            cur = turn_toward(cur, ang(-179.0, 0.0), DEFAULT_MAX_TURN);
        }
        assert!(
            cur.pitch >= -PITCH_LIMIT && cur.pitch <= PITCH_LIMIT,
            "pitch {} escaped the limit",
            cur.pitch
        );
    }

    #[test]
    fn a_lower_cap_makes_a_visibly_slower_bot() {
        let desired = ang(0.0, 120.0);
        let fast = turn_toward(ang(0.0, 0.0), desired, 20.0);
        let slow = turn_toward(ang(0.0, 0.0), desired, 3.0);
        assert!(
            fast.yaw > slow.yaw,
            "a higher cap must turn further in one tick ({} vs {})",
            fast.yaw,
            slow.yaw
        );
        assert!((f64::from(slow.yaw) - 3.0).abs() < 1e-6);
    }

    #[test]
    fn already_on_target_does_not_drift() {
        let a = ang(12.0, -45.0);
        let after = turn_toward(a, a, DEFAULT_MAX_TURN);
        assert!(aim_error(after, a) < 1e-6);
    }

    #[test]
    fn aim_error_is_symmetric_and_zero_on_target() {
        let a = ang(5.0, 10.0);
        let b = ang(-5.0, 40.0);
        assert!((aim_error(a, b) - aim_error(b, a)).abs() < 1e-9);
        assert!(aim_error(a, a) < 1e-12);
    }

    #[test]
    fn compensate_doubles_the_punch() {
        // The whole point: once via pev->v_angle, once in UTIL_MakeVectors.
        let punch = ang(-2.0, 0.5);
        let out = compensate(ang(0.0, 90.0), punch);
        assert!((out.pitch - 4.0).abs() < 1e-4, "pitch {} should be +4", out.pitch);
        assert!((out.yaw - 89.0).abs() < 1e-4, "yaw {} should be 89", out.yaw);
        assert_eq!(PUNCH_APPLICATIONS, 2.0);
    }

    #[test]
    fn compensate_is_the_exact_inverse_of_what_the_server_does() {
        for punch in [ang(0.0, 0.0), ang(-3.5, 1.25), ang(1.0, -4.0), ang(-8.0, 0.0)] {
            for desired in [ang(0.0, 0.0), ang(12.0, -170.0), ang(-30.0, 45.0)] {
                let sent = compensate(desired, punch);
                let landed = resolve_shot(sent, punch);
                assert!(
                    aim_error(landed, desired) < 1e-3,
                    "punch {punch:?} desired {desired:?} landed {landed:?}"
                );
            }
        }
    }

    #[test]
    fn zero_punch_changes_nothing() {
        let a = ang(7.0, -33.0);
        assert!(aim_error(compensate(a, ang(0.0, 0.0)), a) < 1e-9);
    }

    #[test]
    fn compensation_cannot_push_pitch_past_the_engine_limit() {
        // A huge downward punch must not ask for a pitch the engine refuses.
        let out = compensate(ang(80.0, 0.0), ang(-45.0, 0.0));
        assert!(out.pitch <= PITCH_LIMIT, "pitch {} escaped", out.pitch);
    }

    #[test]
    fn punch_decay_matches_pm_droppunchangle_exactly() {
        // len -= (10.0 + len * 0.5) * frametime, on the vector length.
        // Pure pitch: len = 4.0, frametime 0.1 -> 4 - (10 + 2)*0.1 = 2.8.
        let after = decay_punch_step(ang(-4.0, 0.0), 0.1);
        assert!((f64::from(after.pitch) + 2.8).abs() < 1e-4, "{}", after.pitch);

        // Mixed axes: the length shrinks, the direction is preserved.
        let before = ang(-3.0, 4.0); // length 5
        let after = decay_punch_step(before, 0.1);
        let len = (f64::from(after.pitch).powi(2) + f64::from(after.yaw).powi(2)).sqrt();
        assert!((len - (5.0 - (10.0 + 2.5) * 0.1)).abs() < 1e-4, "len {len}");
        // Same direction: pitch:yaw stays -3:4.
        assert!(
            (f64::from(after.pitch) / f64::from(after.yaw) + 0.75).abs() < 1e-5,
            "direction drifted: {after:?}"
        );
    }

    #[test]
    fn punch_decays_to_zero_and_never_goes_negative() {
        let mut p = ang(-12.0, 3.0);
        for _ in 0..200 {
            p = decay_punch_step(p, 0.05);
            assert!(p.pitch.is_finite() && p.yaw.is_finite());
        }
        assert!(p.pitch.abs() < 1e-6 && p.yaw.abs() < 1e-6, "should have settled: {p:?}");
        // And it stays there rather than flipping sign.
        let still = decay_punch_step(p, 0.05);
        assert_eq!(still, Angles::default());
    }

    #[test]
    fn prediction_shrinks_the_punch_monotonically() {
        let p0 = ang(-6.0, 0.0);
        let p1 = predict_punch(p0, 0.05, 1);
        let p2 = predict_punch(p0, 0.05, 2);
        assert!(p1.pitch > p0.pitch, "must shrink toward zero");
        assert!(p2.pitch > p1.pitch);
        assert_eq!(predict_punch(p0, 0.05, 0), p0, "zero frames is a no-op");
    }

    #[test]
    fn predicting_the_decay_reduces_over_compensation() {
        // The stale punch is bigger than the one in force when the shot lands.
        // Compensating with the stale value overshoots; predicting first does
        // not. Model: 2 frames of latency at 50 ms.
        let stale = ang(-6.0, 0.0);
        let real_at_fire = predict_punch(stale, 0.05, 2);

        let naive = compensate(ang(0.0, 0.0), stale);
        let predicted = compensate(ang(0.0, 0.0), real_at_fire);

        let naive_err = aim_error(resolve_shot(naive, real_at_fire), ang(0.0, 0.0));
        let pred_err = aim_error(resolve_shot(predicted, real_at_fire), ang(0.0, 0.0));
        assert!(pred_err < naive_err, "prediction {pred_err} should beat naive {naive_err}");
        assert!(pred_err < 1e-3);
    }

    /// The spring must behave the way a head does, and the numbers are
    /// predictions made before it was written -- so this is a check, not a
    /// restatement of whatever the code happens to do.
    ///
    /// Navigation: a 90-degree turn accelerates hard, does NOT overshoot, and
    /// settles in about half a second. Combat: faster, and deliberately
    /// overshoots by a real margin before coming back -- that is the flick.
    #[test]
    fn the_view_accelerates_overshoots_in_combat_and_settles() {
        let hz = 30.0;
        let dt = 1.0 / hz;

        // Returns (peak deg/s, overshoot deg, seconds to settle inside 1 deg).
        let swing = |gains: SpringGains| {
            let mut m = ViewMotion::default();
            let mut cur = Angles { pitch: 0.0, yaw: 0.0 };
            let target = Angles { pitch: 0.0, yaw: 90.0 };
            let (mut peak, mut overshoot, mut settled) = (0.0f64, 0.0f64, None);
            for i in 0..120 {
                cur = m.step(cur, target, gains, dt);
                peak = peak.max(m.yaw_vel.abs());
                overshoot = overshoot.max(f64::from(cur.yaw) - 90.0);
                if settled.is_none() && (f64::from(cur.yaw) - 90.0).abs() < 1.0 {
                    settled = Some((i + 1) as f64 * dt);
                }
            }
            (peak, overshoot, settled)
        };

        let (nav_peak, nav_over, nav_settle) = swing(NAV_GAINS);
        let (cbt_peak, cbt_over, cbt_settle) = swing(COMBAT_GAINS);
        eprintln!(
            "nav:    peak {nav_peak:.0} deg/s  overshoot {nav_over:.1}  settle {:.2}s",
            nav_settle.unwrap_or(f64::NAN)
        );
        eprintln!(
            "combat: peak {cbt_peak:.0} deg/s  overshoot {cbt_over:.1}  settle {:.2}s",
            cbt_settle.unwrap_or(f64::NAN)
        );

        // It accelerates: a head does not cross 90 degrees at a crawl.
        assert!(nav_peak > 300.0, "nav peak only {nav_peak:.0} deg/s");
        assert!(cbt_peak > nav_peak, "combat should be the faster swing");

        // Navigation does not overshoot -- walking somewhere is not a flick.
        assert!(nav_over < 1.0, "nav overshot by {nav_over:.1} deg");

        // Combat does, and by an amount you could see.
        assert!(
            cbt_over > 5.0,
            "combat overshoot only {cbt_over:.1} deg -- the flick is the point"
        );

        // Both arrive promptly. A view that takes a second to come round is not
        // a human either.
        assert!(nav_settle.unwrap() < 1.0, "nav settle {:?}", nav_settle);
        assert!(cbt_settle.unwrap() < 1.0, "combat settle {:?}", cbt_settle);
    }

    /// A stall must not be integrated as one giant step.
    #[test]
    fn a_long_hitch_does_not_launch_the_view_across_the_map() {
        let mut m = ViewMotion::default();
        let cur = Angles { pitch: 0.0, yaw: 0.0 };
        let target = Angles { pitch: 0.0, yaw: 90.0 };
        // Half a second of stall, handed in as one tick.
        let after = m.step(cur, target, NAV_GAINS, 0.5);
        let moved = f64::from(after.yaw).abs();
        assert!(
            moved < 90.0,
            "a 0.5 s hitch moved the view {moved:.0} deg -- dt must be clamped"
        );
    }

    /// Pitch has no deadband; yaw does. That asymmetry is the whole point.
    ///
    /// Inside a degree, yaw SNAPS and zeroes its velocity -- a settled head does
    /// not jitter left and right. Pitch never snaps, so the same sub-degree
    /// error still produces motion, and the eye-height bob of walking keeps it
    /// excited. Without that the crosshair goes dead, which is what 42.7% of
    /// consecutive live samples sharing an identical integer yaw looked like.
    ///
    /// Note a static target is not the interesting case: a damped spring
    /// settles, correctly. The claim under test is about the DEADBAND, so the
    /// target here is inside it on both axes.
    #[test]
    fn yaw_snaps_inside_the_deadband_and_pitch_does_not() {
        let mut m = ViewMotion::default();
        let mut cur = Angles { pitch: 0.0, yaw: 0.0 };
        // Both errors are under YAW_DEADBAND.
        let target = Angles { pitch: 0.4, yaw: 0.4 };

        let after = m.step(cur, target, NAV_GAINS, 1.0 / 30.0);
        assert_eq!(m.yaw_vel, 0.0, "yaw kept momentum inside the deadband");
        assert_eq!(after.yaw, target.yaw, "yaw should have snapped, not eased");
        assert!(
            after.pitch != cur.pitch && after.pitch != target.pitch,
            "pitch snapped or stalled: {} -> {}",
            cur.pitch,
            after.pitch
        );

        // And under continuous excitation -- which is what walking supplies --
        // the pitch axis keeps moving rather than going still.
        cur = after;
        let mut moves = 0;
        let mut last = cur.pitch;
        for i in 0..60 {
            let bob = Angles { pitch: 0.4 + ((i as f32) * 0.5).sin() * 0.8, yaw: 0.4 };
            cur = m.step(cur, bob, NAV_GAINS, 1.0 / 30.0);
            if (cur.pitch - last).abs() > 1e-4 {
                moves += 1;
            }
            last = cur.pitch;
        }
        assert!(moves > 50, "pitch went still under excitation after {moves} ticks");
    }

    /// Walking at 170 deg with the destination behind at -170: the short way
    /// round swings through the back of the head (through 180, away from the
    /// direction of travel). A human head leads the body, so the spring must
    /// take the long way through the front -- the direction of travel.
    ///
    /// The travel bearing is a hair off due forward. That is not a corner
    /// case, it is the correct ones: with travel exactly 0 the pair sits
    /// smack on the travel axis and YaPB's `fzero(forward)` skips the guard --
    /// there is no straddle to protect -- and moved further round the circle
    /// the two angles stop straddling the travel direction. The guard fires
    /// in a narrow band of bearings where heading, eyes and target conspire.
    #[test]
    fn the_back_swing_guard_forces_the_long_way_through_the_front() {
        let travel = 0.5; // moving towards +x, a whisker off
        let current = Angles { pitch: 0.0, yaw: 170.0 };
        let desired = Angles { pitch: 0.0, yaw: -170.0 };
        let dt = 1.0 / 30.0;

        let mut plain = ViewMotion::default();
        let mut guarded = ViewMotion::default();
        let p = plain.step(current, desired, NAV_GAINS, dt);
        let g = guarded.step_guarded(current, desired, NAV_GAINS, dt, travel);

        // Plain: short way, yaw climbs toward the back (180) and the wrap.
        assert!(f64::from(p.yaw) > 170.0, "plain took the back way: {p:?}");
        // Guarded: yaw falls through 90 and 0 -- the front, at 0.5.
        assert!(f64::from(g.yaw) < 170.0, "guard took the short way: {g:?}");
        // And it did not teleport: the spring still moves one tick at a time.
        assert!(f64::from(g.yaw) > 90.0, "guard jumped, not swung: {g:?}");

        // Follow it through: it must actually pass through the front instead of
        // being dragged round the back by the deadband or the wrap.
        let mut cur = current;
        let mut hit_front = false;
        for _ in 0..600 {
            cur = guarded.step_guarded(cur, desired, NAV_GAINS, dt, travel);
            if (f64::from(cur.yaw) - travel).abs() < 5.0 {
                hit_front = true;
                break;
            }
        }
        assert!(hit_front, "the long way round never crossed the front");
    }

    /// When current and desired do not straddle the direction of travel, the
    /// guard must change nothing: a head that is already turning the right way
    /// just keeps turning.
    #[test]
    fn the_guard_is_inert_when_the_short_way_is_forward() {
        let travel = 90.0; // moving towards +y
        let current = Angles { pitch: 0.0, yaw: 40.0 };
        let desired = Angles { pitch: 0.0, yaw: 70.0 };
        let dt = 1.0 / 30.0;

        let mut plain = ViewMotion::default();
        let mut guarded = ViewMotion::default();
        let p = plain.step(current, desired, NAV_GAINS, dt);
        let g = guarded.step_guarded(current, desired, NAV_GAINS, dt, travel);
        assert_eq!(p, g, "guard moved a head that was already turning forward");
    }
}
