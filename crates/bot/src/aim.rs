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
}
