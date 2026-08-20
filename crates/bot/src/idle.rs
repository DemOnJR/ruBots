//! Not getting kicked for being idle.
//!
//! ## The predicate, exactly
//!
//! `CCSPlayer::CheckActivityInGame` (`dlls/API/CSPlayer.cpp:530-540`) is the
//! whole of it:
//!
//! ```text
//! const float deltaYaw   = (m_vecOldvAngle.y - pev->v_angle.y);
//! const float deltaPitch = (m_vecOldvAngle.x - pev->v_angle.x);
//! m_vecOldvAngle = pev->v_angle;
//! return (fabs(deltaYaw) >= 0.1f && fabs(deltaPitch) >= 0.1f);
//! ```
//!
//! Two things about it are load-bearing and easy to get wrong:
//!
//! * It is an **`&&`**, not an `||`. Sweeping the yaw while holding the pitch
//!   perfectly still counts as *idle*. Both axes must move.
//! * `m_vecOldvAngle` is only rewritten **when the check runs**, and the check
//!   runs on a 5-second timer: `m_flIdleCheckTime = gpGlobals->time + 5.0`
//!   (`dlls/player.cpp:4765-4774`). So the deltas compared are between angles
//!   5 seconds apart, not between consecutive ticks. Jitter that wobbles
//!   quickly but returns to where it started reads as perfectly stationary.
//!
//! Failing it long enough calls `DropIdlePlayer("Player idle")`
//! (`dlls/player.cpp:4779-4783`), and note the guard there is `!IsBot()` — a
//! third-party client is not a `IsBot()` bot, so this applies to us.
//!
//! ## Why a sawtooth, and not a sine
//!
//! The bot cannot see the sampler's phase, so the jitter has to satisfy
//! `|f(t + 5) - f(t)| >= 0.1` for **every** `t`, not on average.
//!
//! A sine fails: `f(t) = A sin(2*pi*t/P)` gives
//! `f(t+5) - f(t) = 2A sin(5*pi/P) cos(2*pi*(t+2.5)/P)`, which is zero wherever
//! that cosine is. So does any triangle wave — near a turning point the two
//! samples straddle the peak and cancel. Anything that comes back to where it
//! started has a phase at which it looks frozen.
//!
//! A wrapping ramp does not. Over any 5-second window the value has either
//! advanced by `amp * 5 / period`, or wrapped once and advanced by
//! `amp * (5 / period - 1)`. Both are constants, so if both exceed 0.1 the
//! predicate holds unconditionally — which is what [`AntiIdle::guarantees`]
//! checks and what the tests sweep. The only requirement is `period > 5`, so a
//! window can never contain two wraps.

use crate::math::{norm_angle, Angles};

/// How often `CheckActivityInGame` runs (`dlls/player.cpp:4768`).
pub const IDLE_CHECK_INTERVAL: f32 = 5.0;

/// The threshold each axis must clear (`dlls/API/CSPlayer.cpp:539`).
pub const IDLE_ANGLE_EPSILON: f32 = 0.1;

/// One wrapping ramp: amplitude and period.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ramp {
    pub amplitude: f32,
    pub period: f32,
}

impl Ramp {
    /// Offset at phase `t` seconds. Centred on zero so the mean aim is
    /// unchanged.
    pub fn at(self, t: f32) -> f32 {
        if self.period <= 0.0 {
            return 0.0;
        }
        let frac = (t / self.period).rem_euclid(1.0);
        self.amplitude * (frac - 0.5)
    }

    /// The two possible magnitudes of a `window`-second delta: the plain
    /// advance, and the advance across a wrap.
    fn deltas(self, window: f32) -> (f32, f32) {
        let advance = self.amplitude * window / self.period;
        (advance.abs(), (advance - self.amplitude).abs())
    }

    /// True when *every* `window`-long delta clears `epsilon`.
    pub fn guarantees(self, window: f32, epsilon: f32) -> bool {
        if self.period <= window {
            // Two wraps could fall inside one window and cancel.
            return false;
        }
        let (plain, wrapped) = self.deltas(window);
        plain >= epsilon && wrapped >= epsilon
    }
}

/// A slow drift on both axes that keeps `CheckActivityInGame` satisfied.
///
/// The amplitudes are around a degree — invisible in play, but a hundred times
/// the threshold, so rounding on the wire cannot eat it. The periods are
/// deliberately different and not multiples of 5, so the two axes do not
/// wrap together and the motion does not look mechanical.
///
/// **Chosen, not recovered.** Only the constraint they satisfy is verified.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AntiIdle {
    pub yaw: Ramp,
    pub pitch: Ramp,
    /// Seconds accumulated since the bot spawned.
    pub phase: f32,
}

impl Default for AntiIdle {
    fn default() -> Self {
        Self {
            yaw: Ramp { amplitude: 1.0, period: 12.0 },
            pitch: Ramp { amplitude: 0.6, period: 9.0 },
            phase: 0.0,
        }
    }
}

impl AntiIdle {
    /// A drift that belongs to this bot and no other.
    ///
    /// The shared configuration is the reason thirty bots sweep the same
    /// sawtooth in phase -- a tell on its own. Every parameter here is drawn
    /// from the bot's seed, across windows that provably satisfy
    /// [`AntiIdle::guarantees`]: amplitude floors chosen so even the slowest
    /// period clears `IDLE_ANGLE_EPSILON` at `IDLE_CHECK_INTERVAL`, and
    /// period ceilings chosen so a window can never hold two wraps.
    ///
    /// The RNG is a plain LCG on the seed; the values are parameters, not
    /// security.
    pub fn from_seed(seed: u64) -> Self {
        let mut h = seed;
        let next = |h: &mut u64| {
            *h = h.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((*h >> 33) as f64 / (1u64 << 31) as f64) as f32
        };
        // Windows that satisfy `guarantees(): yaw amp >= 0.9, period <= 16 ->
        // plain delta >= 0.28, wrapped >= 0.62; pitch amp >= 0.6, period <= 13
        // -> plain >= 0.23, wrapped >= 0.37. All well above the 0.1 threshold.
        Self {
            yaw: Ramp { amplitude: 0.9 + next(&mut h) * 0.5, period: 8.0 + next(&mut h) * 8.0 },
            pitch: Ramp { amplitude: 0.6 + next(&mut h) * 0.4, period: 7.0 + next(&mut h) * 6.0 },
            phase: next(&mut h) * 8.0,
        }
    }

    pub fn advance(&mut self, dt: f32) {
        self.phase += dt;
        // Keep the accumulator small so f32 precision never erodes the ramp.
        // The reduction must be a whole number of periods on *both* ramps or it
        // introduces a discontinuity — which would be a phase at which the
        // 5-second delta collapses, i.e. exactly the failure this module
        // exists to prevent. The product of the two periods is such a number.
        let cycle = self.yaw.period * self.pitch.period;
        if cycle > 0.0 && self.phase > cycle {
            self.phase -= cycle;
        }
    }

    /// The offset to add to the bot's aim right now.
    pub fn offset(&self) -> Angles {
        Angles { pitch: self.pitch.at(self.phase), yaw: self.yaw.at(self.phase) }
    }

    /// Apply the drift to an angle.
    /// The largest offset either ramp can reach, at any phase.
    ///
    /// The ramp is centred, so this is half its amplitude. Callers that need to
    /// reason about the aim exactly -- tests, and anything checking a firing
    /// cone -- need a bound on how far the drift can move it.
    pub fn max_offset(&self) -> Angles {
        Angles {
            pitch: self.pitch.amplitude / 2.0,
            yaw: self.yaw.amplitude / 2.0,
        }
    }

    pub fn apply(&self, base: Angles) -> Angles {
        let o = self.offset();
        Angles {
            pitch: (base.pitch + o.pitch).clamp(-crate::aim::PITCH_LIMIT, crate::aim::PITCH_LIMIT),
            yaw: norm_angle(f64::from(base.yaw + o.yaw)) as f32,
        }
    }

    /// True when this configuration provably passes the check at every phase.
    ///
    /// Not a test helper: the controller can assert it at construction, so a
    /// mis-tuned amplitude is a loud failure rather than a bot that quietly
    /// gets kicked twenty minutes in.
    pub fn guarantees(&self) -> bool {
        self.yaw.guarantees(IDLE_CHECK_INTERVAL, IDLE_ANGLE_EPSILON)
            && self.pitch.guarantees(IDLE_CHECK_INTERVAL, IDLE_ANGLE_EPSILON)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ReGameDLL's predicate, transcribed verbatim.
    fn check_activity(old: Angles, now: Angles) -> bool {
        let delta_yaw = old.yaw - now.yaw;
        let delta_pitch = old.pitch - now.pitch;
        delta_yaw.abs() >= IDLE_ANGLE_EPSILON && delta_pitch.abs() >= IDLE_ANGLE_EPSILON
    }

    #[test]
    fn the_predicate_is_a_conjunction_not_a_disjunction() {
        // dlls/API/CSPlayer.cpp:539 — `&&`. Moving one axis is not enough.
        let base = Angles { pitch: 0.0, yaw: 0.0 };
        assert!(!check_activity(base, Angles { pitch: 0.0, yaw: 90.0 }), "yaw alone is idle");
        assert!(!check_activity(base, Angles { pitch: 45.0, yaw: 0.0 }), "pitch alone is idle");
        assert!(check_activity(base, Angles { pitch: 0.1, yaw: 0.1 }), "both, exactly at the bound");
        assert!(
            !check_activity(base, Angles { pitch: 0.09, yaw: 5.0 }),
            "just under on one axis is idle"
        );
    }

    #[test]
    fn the_jitter_passes_the_check_at_every_sampling_phase() {
        // The sampler's phase is unknown, so sweep it finely and simulate the
        // real thing: sample, compare with the last sample, 5 seconds apart.
        let cfg = AntiIdle::default();
        assert!(cfg.guarantees(), "the configuration must be provably safe");

        let mut worst_yaw = f32::INFINITY;
        let mut worst_pitch = f32::INFINITY;

        for step in 0..2000 {
            let phase0 = step as f32 * 0.0173; // an irrational-ish sweep
            let mut idle = AntiIdle { phase: phase0, ..AntiIdle::default() };
            let mut old = idle.apply(Angles::default());

            // Twenty consecutive 5-second windows from this starting phase.
            // 20 * 5 = 100 s, which from the top of the phase sweep carries
            // past the 108 s accumulator reduction in `advance` — so the sweep
            // proves that reduction is seamless too.
            for window in 0..20 {
                // Advance in realistic 50 ms ticks, not one jump.
                for _ in 0..100 {
                    idle.advance(0.05);
                }
                let now = idle.apply(Angles::default());
                assert!(
                    check_activity(old, now),
                    "idle at phase {phase0} window {window}: {old:?} -> {now:?}"
                );
                worst_yaw = worst_yaw.min((old.yaw - now.yaw).abs());
                worst_pitch = worst_pitch.min((old.pitch - now.pitch).abs());
                old = now;
            }
        }

        assert!(
            worst_yaw >= IDLE_ANGLE_EPSILON && worst_pitch >= IDLE_ANGLE_EPSILON,
            "tightest margins: yaw {worst_yaw}, pitch {worst_pitch}"
        );
    }

    #[test]
    fn per_seed_drifts_stay_safe_and_differ() {
        // The companion to W7: the fleet must not sweep the same sawtooth in
        // phase. Every seed must produce a provably safe, deterministic drift,
        // and 64 seeds must not collapse into a handful of configurations.
        let mut tuples = std::collections::HashSet::new();
        for seed in 0..64 {
            let a = AntiIdle::from_seed(seed);
            let b = AntiIdle::from_seed(seed);
            assert!(a.guarantees(), "seed {seed}: a drift that is not provably safe");
            assert_eq!(a, b, "seed {seed}: the drift must be deterministic");
            tuples.insert((
                (a.yaw.amplitude * 1000.0) as u32,
                (a.yaw.period * 100.0) as u32,
                (a.pitch.amplitude * 1000.0) as u32,
                (a.pitch.period * 100.0) as u32,
            ));
        }
        assert!(
            tuples.len() > 48,
            "64 seeds collapsed into {} distinct drifts",
            tuples.len()
        );
    }

    #[test]
    fn a_sine_would_have_failed_this_test() {
        // Not a hypothetical: this is why the ramp is a ramp. A sinusoid has
        // phases where two samples 5 s apart are identical.
        let amp = 1.0f32;
        let period = 20.0f32;
        let f = |t: f32| amp * (std::f32::consts::TAU * t / period).sin();
        let bad = (0..2000).any(|i| {
            let t = i as f32 * 0.01;
            (f(t + IDLE_CHECK_INTERVAL) - f(t)).abs() < IDLE_ANGLE_EPSILON
        });
        assert!(bad, "a sine must have a dead phase — that is the point");
    }

    #[test]
    fn a_period_shorter_than_the_window_is_rejected() {
        // Two wraps inside one window can cancel, so refuse to claim safety.
        let r = Ramp { amplitude: 1.0, period: 3.0 };
        assert!(!r.guarantees(IDLE_CHECK_INTERVAL, IDLE_ANGLE_EPSILON));
        let r = Ramp { amplitude: 1.0, period: 5.0 };
        assert!(!r.guarantees(IDLE_CHECK_INTERVAL, IDLE_ANGLE_EPSILON), "equal is not enough");
    }

    #[test]
    fn a_tiny_amplitude_is_rejected() {
        // amp * 5/12 = 0.04 < 0.1
        let r = Ramp { amplitude: 0.1, period: 12.0 };
        assert!(!r.guarantees(IDLE_CHECK_INTERVAL, IDLE_ANGLE_EPSILON));
    }

    #[test]
    fn the_drift_stays_small_enough_not_to_spoil_aim() {
        let mut idle = AntiIdle::default();
        let mut max = 0.0f32;
        for _ in 0..4000 {
            idle.advance(0.05);
            let o = idle.offset();
            max = max.max(o.pitch.abs()).max(o.yaw.abs());
        }
        assert!(max <= 0.51, "drift {max} deg is too big to be invisible");
    }

    #[test]
    fn the_phase_accumulator_does_not_run_away() {
        let mut idle = AntiIdle::default();
        for _ in 0..200_000 {
            idle.advance(0.05);
        }
        assert!(idle.phase.is_finite());
        assert!(idle.phase <= idle.yaw.period * idle.pitch.period + 1.0, "{}", idle.phase);
    }

    #[test]
    fn applying_the_drift_respects_the_pitch_limit_and_the_yaw_wrap() {
        let idle = AntiIdle { phase: 3.0, ..AntiIdle::default() };
        let a = idle.apply(Angles { pitch: 89.0, yaw: 179.9 });
        assert!(a.pitch <= crate::aim::PITCH_LIMIT);
        assert!((-180.0..180.0).contains(&a.yaw), "yaw {} not normalised", a.yaw);
    }

    #[test]
    fn a_degenerate_ramp_is_inert_rather_than_nan() {
        let r = Ramp { amplitude: 1.0, period: 0.0 };
        assert_eq!(r.at(5.0), 0.0);
        assert!(!r.guarantees(IDLE_CHECK_INTERVAL, IDLE_ANGLE_EPSILON));
    }
}
