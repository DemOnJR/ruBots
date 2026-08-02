//! Vector and angle math shared by the AI.
//!
//! Port of `internal/bot/util.go` plus the aiming half of `think.go`.
//! Verified constants:
//!
//! * `normAngle` (`0x140704440`) is `mod(mod(a, 360) + 540, 360) - 180`
//!   — the `360.0`, `540.0` and `180.0` operands are at `0x1411491D8`,
//!   `0x1411491F0` and `0x141149050`
//! * `aimAt` (`0x140702DE0`) adds a **17.0** view-height offset to the
//!   shooter's z (`0x141149160`) before taking `atan2`, which is the CS 1.6
//!   standing eye offset (Half-Life's is 28)
//! * yaw is `atan2(dy, dx)`, pitch is `atan2(dz, hypot(dx, dy))`, both
//!   converted with `180/PI` (`math.atan2` then `math.archHypot`)

/// Eye height above the entity origin.
pub const VIEW_HEIGHT: f32 = 17.0;

/// A point or direction in world space.
pub type Vec3 = [f32; 3];

/// Yaw/pitch pair in degrees, as the engine orders them: `[pitch, yaw]`
/// matches `viewangles[0]`/`viewangles[1]` on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Angles {
    pub pitch: f32,
    pub yaw: f32,
}

/// Fold an angle into `[-180, 180)`.
pub fn norm_angle(a: f64) -> f64 {
    ((a % 360.0) + 540.0) % 360.0 - 180.0
}

pub fn sub(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

pub fn add(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

/// Full 3D length.
pub fn length(v: Vec3) -> f32 {
    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
}

/// Horizontal length, ignoring z — `norm2` in the original.
pub fn length2d(v: Vec3) -> f32 {
    (v[0] * v[0] + v[1] * v[1]).sqrt()
}

/// Distance between two points.
pub fn distance(a: Vec3, b: Vec3) -> f32 {
    length(sub(a, b))
}

/// Horizontal distance between two points.
pub fn distance2d(a: Vec3, b: Vec3) -> f32 {
    length2d(sub(a, b))
}

/// The eye position for an entity standing at `origin`.
pub fn eye_position(origin: Vec3) -> Vec3 {
    [origin[0], origin[1], origin[2] + VIEW_HEIGHT]
}

/// Angles that point from `from` (an origin, not an eye) at `target`.
///
/// The view-height offset is applied to the shooter exactly as `aimAt` does.
pub fn aim_angles(from: Vec3, target: Vec3) -> Angles {
    let eye = eye_position(from);
    let d = sub(target, eye);
    let yaw = (f64::from(d[1])).atan2(f64::from(d[0])).to_degrees();
    let flat = f64::from(length2d(d));
    let pitch = (f64::from(d[2])).atan2(flat).to_degrees();
    Angles {
        // The engine's pitch is inverted relative to the maths convention:
        // looking up is a negative viewangles[0].
        pitch: norm_angle(-pitch) as f32,
        yaw: norm_angle(yaw) as f32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn norm_angle_folds_into_signed_range() {
        assert!((norm_angle(0.0) - 0.0).abs() < 1e-9);
        assert!((norm_angle(90.0) - 90.0).abs() < 1e-9);
        assert!((norm_angle(180.0) + 180.0).abs() < 1e-9);
        assert!((norm_angle(190.0) + 170.0).abs() < 1e-9);
        assert!((norm_angle(370.0) - 10.0).abs() < 1e-9);
        assert!((norm_angle(-190.0) - 170.0).abs() < 1e-9);
        assert!((norm_angle(-3600.0)).abs() < 1e-9);
    }

    #[test]
    fn norm_angle_output_is_always_in_range() {
        let mut a = -2000.0f64;
        while a < 2000.0 {
            let n = norm_angle(a);
            assert!((-180.0..180.0).contains(&n), "{a} -> {n} out of range");
            a += 7.3;
        }
    }

    #[test]
    fn view_height_is_the_cs_value() {
        assert_eq!(VIEW_HEIGHT, 17.0);
        assert_eq!(eye_position([1.0, 2.0, 3.0]), [1.0, 2.0, 20.0]);
    }

    #[test]
    fn yaw_points_along_the_axes() {
        let me = [0.0, 0.0, 0.0];
        // Straight along +x at eye level.
        let a = aim_angles(me, [100.0, 0.0, VIEW_HEIGHT]);
        assert!(close(a.yaw, 0.0), "yaw {} should be 0", a.yaw);
        assert!(close(a.pitch, 0.0), "pitch {} should be 0", a.pitch);

        let a = aim_angles(me, [0.0, 100.0, VIEW_HEIGHT]);
        assert!(close(a.yaw, 90.0), "yaw {} should be 90", a.yaw);

        let a = aim_angles(me, [-100.0, 0.0, VIEW_HEIGHT]);
        assert!(close(a.yaw.abs(), 180.0), "yaw {} should be +/-180", a.yaw);
    }

    #[test]
    fn looking_up_gives_negative_pitch() {
        // Target directly above eye level, 100 units out and 100 units up.
        let a = aim_angles([0.0, 0.0, 0.0], [100.0, 0.0, VIEW_HEIGHT + 100.0]);
        assert!(a.pitch < 0.0, "looking up must be negative pitch, got {}", a.pitch);
        assert!(close(a.pitch, -45.0), "pitch {} should be -45", a.pitch);
    }

    #[test]
    fn looking_down_gives_positive_pitch() {
        let a = aim_angles([0.0, 0.0, 0.0], [100.0, 0.0, VIEW_HEIGHT - 100.0]);
        assert!(close(a.pitch, 45.0), "pitch {} should be +45", a.pitch);
    }

    #[test]
    fn a_target_at_our_own_feet_does_not_produce_nan() {
        let a = aim_angles([0.0, 0.0, 0.0], [0.0, 0.0, 0.0]);
        assert!(a.pitch.is_finite() && a.yaw.is_finite());
    }

    #[test]
    fn distances_ignore_z_when_flat() {
        let a = [0.0, 0.0, 0.0];
        let b = [3.0, 4.0, 100.0];
        assert!(close(distance2d(a, b), 5.0));
        assert!(close(distance(a, b), (25.0f32 + 10000.0).sqrt()));
    }
}
