//! Pacing the outgoing `clc_move` stream honestly.
//!
//! Every `usercmd_t` claims `msec` milliseconds of game time, and the server
//! integrates movement over it. That makes `msec` an assertion about the real
//! world, and ReHLDS checks it two independent ways:
//!
//! * **`CUserCmdTimeLimiter::CheckLimits`** (`rehlds/rehlds/rehlds_security.cpp:214`)
//!   accumulates our `msec` into `ust->msecTime` and compares it against wall
//!   clock. `error = msecTime - now` in milliseconds; when `error > 300` **and**
//!   the recent ratio of claimed-to-real time exceeds 3.0, the command is
//!   **discarded**. At the default `sv_rehlds_movecmdtime_max_warnings = -1`
//!   nobody is kicked and nothing is logged — the movement simply never happens.
//! * **`SV_CheckCmdTimes`** (`rehlds/engine/sv_main.cpp:8030`), once a second:
//!   when `connecttime + cmdtime - realtime > clockwindow` (0.5 s) it sets
//!   `ignorecmdtime`, and every command for the next half second is dropped.
//!
//! The previous driver sent a fixed `msec = 20` from a busy loop running at
//! ~200 packets/s. That claims about four seconds of game time per real second:
//! ratio 4.0, error growing 3 s every second. **Essentially every movement
//! command this client ever sent was discarded**, which is why the bot connected
//! happily and never moved.
//!
//! [`MoveClock`] fixes that by construction: it never issues more milliseconds
//! than have actually elapsed, so `error <= 0` always and the ratio sits at 1.0.

use std::time::{Duration, Instant};

/// Nominal send rate. A real CS 1.6 client was measured at ~51 packets/s.
pub const TICK: Duration = Duration::from_millis(20);

/// `SV_RunCmd` splits any command longer than this into two halves and zeroes
/// `impulse` on the second (`sv_user.cpp:797-806`). Staying under it keeps one
/// command one command.
pub const MAX_MSEC: u8 = 50;

/// A zero-msec command does no work, and long streaks of them are what
/// `sv_rehlds_movecmd_max_null_streak` exists to catch.
pub const MIN_MSEC: u8 = 1;

/// `ex_interp` in milliseconds. Must be `0..=100` or `CheckLimits` discards the
/// command outright (`rehlds_security.cpp:250-256`, `MAX_EX_INTERP 0.1f`).
pub const LERP_MSEC: i16 = 50;

/// `CMD_MAXBACKUP` is 64 and the server rejects `numcmds + numbackup >= 63`
/// (`sv_user.h:36`, `sv_user.cpp:1634`). We stay far under it.
///
/// Eight rather than a handful because this bounds how fast a stall is repaid:
/// after a 400 ms hiccup the server's `msecTime` is 400 ms behind wall clock,
/// which is inside the slow-mo window, and the sooner we are level again the
/// shorter that excursion lasts. Eight × 50 ms clears any plausible stall in a
/// single packet.
pub const MAX_CMDS_PER_PACKET: usize = 8;

/// Issues `msec` values that never outrun the wall clock.
#[derive(Debug, Clone)]
pub struct MoveClock {
    /// Reset when the server (re)spawns us. `SV_WriteSpawn` sets
    /// `connecttime = realtime` and `cmdtime = 0` together
    /// (`sv_main.cpp:1476-1479`), so this must track that moment.
    epoch: Instant,
    /// Total game time we have claimed so far.
    issued_ms: u64,
}

impl MoveClock {
    pub fn new(now: Instant) -> Self {
        Self {
            epoch: now,
            issued_ms: 0,
        }
    }

    /// Restart the accounting, on `svc_signonnum` or a level change.
    pub fn reset(&mut self, now: Instant) {
        self.epoch = now;
        self.issued_ms = 0;
    }

    pub fn issued_ms(&self) -> u64 {
        self.issued_ms
    }

    /// Milliseconds elapsed but not yet claimed. Never negative by
    /// construction, which is exactly the invariant both server checks want.
    pub fn debt_ms(&self, now: Instant) -> u64 {
        let elapsed = now.saturating_duration_since(self.epoch).as_millis() as u64;
        elapsed.saturating_sub(self.issued_ms)
    }

    /// The commands due at `now`, oldest first. Empty when nothing is owed.
    ///
    /// Oldest-first matters: `build_move_payload` writes deltas in slice order
    /// and the server reads them back with `for (i = totalcmds-1; i >= 0; i--)`
    /// (`sv_user.cpp:1638-1641`), so the first on the wire is the oldest.
    pub fn due(&mut self, now: Instant) -> Vec<u8> {
        let mut out = Vec::new();
        while out.len() < MAX_CMDS_PER_PACKET {
            let debt = self.debt_ms(now);
            if debt < u64::from(MIN_MSEC) {
                break;
            }
            let step = debt.min(u64::from(MAX_MSEC)) as u8;
            self.issued_ms += u64::from(step);
            out.push(step);
        }
        out
    }

    /// How long to wait before the next command is due — the socket read
    /// timeout, so the loop blocks instead of spinning.
    pub fn next_due(&self, now: Instant) -> Duration {
        let elapsed = now.saturating_duration_since(self.epoch);
        let target = Duration::from_millis(self.issued_ms) + TICK;
        target.saturating_sub(elapsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A faithful port of the two server-side checks, used as a test oracle.
    ///
    /// The point is that this is *the server's own arithmetic*, not a
    /// restatement of what we hope it does — a test written against our own
    /// assumptions could not have caught the bug this module exists to fix.
    mod server {
        /// `sv_rehlds_movecmdtime_*` defaults, `rehlds_security.cpp:16-21`.
        pub const SAMPLES: u64 = 120;
        pub const MAX_ERROR: f64 = 300.0;
        pub const MAX_SCALE: f64 = 3.0;
        pub const MIN_SCALE: f64 = 0.5;
        /// `clockwindow`, `net_ws.cpp:83`.
        pub const CLOCK_WINDOW: f64 = 0.5;

        /// `CUserCmdTimeLimiter::CheckLimits`, `rehlds_security.cpp:214-364`.
        #[derive(Default)]
        pub struct TimeLimiter {
            pub msec_time: u64,
            pub last_update: u64,
            pub avg_msec: f64,
            pub avg_server_time: f64,
            pub num_frames: u64,
            pub discarded: u32,
        }

        impl TimeLimiter {
            /// `true` == the command is discarded.
            pub fn check(&mut self, now_ms: u64, msec: u8, lerp_msec: i16) -> bool {
                if !(0..=100).contains(&lerp_msec) {
                    self.discarded += 1;
                    return true;
                }
                if self.msec_time == 0 {
                    self.msec_time = now_ms;
                }
                if self.last_update == 0 {
                    self.last_update = now_ms;
                }
                self.msec_time += u64::from(msec);

                if self.num_frames < SAMPLES {
                    self.avg_msec += f64::from(msec);
                    self.avg_server_time += (now_ms - self.last_update) as f64;
                    self.num_frames += 1;
                } else {
                    self.avg_msec /= SAMPLES as f64;
                    self.avg_server_time /= SAMPLES as f64;
                    self.num_frames = 0;
                }
                self.last_update = now_ms;

                // error = msecTime - now, in milliseconds.
                let error = self.msec_time as f64 - now_ms as f64;
                let ratio = if self.avg_msec != 0.0 && self.avg_server_time != 0.0 {
                    self.avg_msec / self.avg_server_time
                } else {
                    0.0
                };

                let abuse = if error > MAX_ERROR {
                    ratio > MAX_SCALE
                } else if error < -MAX_ERROR {
                    if error < -(MAX_ERROR * 2.0) {
                        self.msec_time = now_ms;
                    }
                    ratio < MIN_SCALE
                } else {
                    false
                };

                if abuse {
                    self.msec_time -= u64::from(msec);
                    self.discarded += 1;
                }
                abuse
            }
        }

        /// `SV_CheckCmdTimes`, `sv_main.cpp:8030-8060`. Runs once a second.
        #[derive(Default)]
        pub struct CmdTimes {
            pub cmdtime: f64,
            pub ignored_until: f64,
            pub armed: u32,
        }

        impl CmdTimes {
            pub fn add(&mut self, msec: u8) {
                self.cmdtime += f64::from(msec) / 1000.0;
            }
            pub fn tick(&mut self, realtime: f64) {
                let dif = self.cmdtime - realtime;
                if dif > CLOCK_WINDOW {
                    self.ignored_until = CLOCK_WINDOW + realtime;
                    self.cmdtime = realtime;
                    self.armed += 1;
                } else if dif < -CLOCK_WINDOW {
                    self.cmdtime = realtime;
                }
            }
        }
    }

    /// Drive the clock through simulated time, feeding both server checks.
    fn simulate(frames: impl Iterator<Item = u64>) -> (server::TimeLimiter, server::CmdTimes, u64) {
        let start = Instant::now();
        let mut clock = MoveClock::new(start);
        let mut limiter = server::TimeLimiter::default();
        let mut times = server::CmdTimes::default();

        let mut now_ms = 0u64;
        let mut next_second = 1000u64;
        let mut sent = 0u64;

        for step in frames {
            now_ms += step;
            let now = start + Duration::from_millis(now_ms);
            for msec in clock.due(now) {
                assert!(
                    (MIN_MSEC..=MAX_MSEC).contains(&msec),
                    "msec {msec} outside 1..=50"
                );
                limiter.check(now_ms, msec, LERP_MSEC);
                times.add(msec);
                sent += 1;
            }
            while now_ms >= next_second {
                times.tick(next_second as f64 / 1000.0);
                next_second += 1000;
            }
        }
        (limiter, times, sent)
    }

    /// A steady loop must be *perfectly* clean. Any discard here is a bug in
    /// the accounting, not bad luck.
    #[test]
    fn ten_simulated_minutes_of_steady_ticks_are_never_discarded() {
        let (limiter, times, sent) = simulate(std::iter::repeat(20).take(30_000));
        assert!(sent > 25_000, "only {sent} commands issued");
        assert_eq!(limiter.discarded, 0, "CheckLimits discarded a steady stream");
        assert_eq!(times.armed, 0, "SV_CheckCmdTimes armed on a steady stream");
    }

    /// With real stalls thrown in, a few commands may land inside the server's
    /// slow-mo window while we are catching back up. That is unavoidable -- a
    /// real client alt-tabbing does the same -- so the honest assertion is that
    /// it stays negligible, not that it is zero. Tuning a constant until a
    /// stricter assertion passed would be fitting the test to the code.
    #[test]
    fn ten_simulated_minutes_with_stalls_stay_far_inside_the_servers_tolerance() {
        // A jittery 20 ms frame with occasional long stalls -- GC pauses, a
        // fragmented signon burst, the OS descheduling us.
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let frames = (0..40_000).map(move |i| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            if i % 997 == 0 {
                200 + seed % 200 // a stall of 200-400 ms
            } else {
                15 + seed % 12 // 15-26 ms
            }
        });

        let (limiter, times, sent) = simulate(frames);
        assert!(sent > 20_000, "only {sent} commands issued in ~10 minutes");

        let rate = f64::from(limiter.discarded) / sent as f64;
        assert!(
            rate < 0.001,
            "CheckLimits discarded {}/{sent} commands ({:.3}%) -- movement is being eaten",
            limiter.discarded,
            rate * 100.0
        );
        // This one must be exactly zero: arming ignorecmdtime blackholes half a
        // second of input, which is a visible stutter rather than a lost tick.
        assert_eq!(
            times.armed, 0,
            "SV_CheckCmdTimes armed ignorecmdtime {} times",
            times.armed
        );
    }

    /// The bug this module exists to prevent, reproduced against the same
    /// oracle. If this ever stops failing, the oracle has drifted.
    #[test]
    fn the_old_fixed_msec_busy_loop_is_discarded_by_the_server() {
        let mut limiter = server::TimeLimiter::default();
        let mut times = server::CmdTimes::default();
        let mut next_second = 1000u64;

        // 200 packets/s, each claiming 20 ms: 4 s of game time per real second.
        for i in 0..2_000u64 {
            let now_ms = i * 5;
            limiter.check(now_ms, 20, 20);
            times.add(20);
            while now_ms >= next_second {
                times.tick(next_second as f64 / 1000.0);
                next_second += 1000;
            }
        }
        assert!(
            limiter.discarded > 1_000,
            "expected the speedhack check to eat most commands, ate {}",
            limiter.discarded
        );
        assert!(
            times.armed > 0,
            "expected SV_CheckCmdTimes to arm ignorecmdtime at least once"
        );
    }

    #[test]
    fn claimed_time_never_runs_ahead_of_the_wall_clock() {
        let start = Instant::now();
        let mut clock = MoveClock::new(start);
        for ms in (0..60_000).step_by(7) {
            let now = start + Duration::from_millis(ms);
            clock.due(now);
            assert!(
                clock.issued_ms() <= ms,
                "claimed {} ms at {} ms elapsed",
                clock.issued_ms(),
                ms
            );
        }
    }

    #[test]
    fn a_long_stall_is_repaid_in_bounded_chunks_not_one_huge_command() {
        let start = Instant::now();
        let mut clock = MoveClock::new(start);
        // Nothing for a full second, then ask.
        let cmds = clock.due(start + Duration::from_millis(1_000));
        assert_eq!(cmds.len(), MAX_CMDS_PER_PACKET);
        assert!(cmds.iter().all(|&m| m <= MAX_MSEC));
        // The rest of the debt survives to the following packets rather than
        // being silently dropped or crammed into one over-long command.
        assert!(clock.debt_ms(start + Duration::from_millis(1_000)) > 0);
    }

    #[test]
    fn nothing_is_owed_before_a_tick_has_passed() {
        let start = Instant::now();
        let mut clock = MoveClock::new(start);
        assert!(clock.due(start).is_empty());
        assert!(clock.due(start + Duration::from_micros(500)).is_empty());
        assert_eq!(clock.due(start + Duration::from_millis(5)).len(), 1);
    }

    #[test]
    fn next_due_shrinks_as_the_tick_approaches() {
        let start = Instant::now();
        let mut clock = MoveClock::new(start);
        clock.due(start + Duration::from_millis(20));
        let a = clock.next_due(start + Duration::from_millis(20));
        let b = clock.next_due(start + Duration::from_millis(30));
        assert!(b < a, "next_due should count down: {a:?} then {b:?}");
        assert_eq!(clock.next_due(start + Duration::from_millis(999)), Duration::ZERO);
    }

    #[test]
    fn reset_clears_the_debt() {
        let start = Instant::now();
        let mut clock = MoveClock::new(start);
        clock.due(start + Duration::from_millis(500));
        let later = start + Duration::from_millis(500);
        clock.reset(later);
        assert_eq!(clock.issued_ms(), 0);
        assert_eq!(clock.debt_ms(later), 0);
    }

    #[test]
    fn lerp_msec_is_inside_the_servers_accepted_range() {
        assert!((0..=100).contains(&LERP_MSEC));
    }
}
