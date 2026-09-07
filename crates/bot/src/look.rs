//! Where a bot that is holding position points its crosshair.
//!
//! ## What was wrong
//!
//! The camp behaviour lerped the look target from one point to another across
//! the whole hold, so the crosshair crept without ever stopping, and the
//! anti-idle drift ([`crate::idle`]) rode on top of it. Measured from the
//! bytes actually sent (`cargo run -p client --example look_trace`), a holding
//! bot reversed its yaw **6.7 to 7.7 times a second** and never held an angle
//! for longer than **0.6 s**. That is a tremor. A person parks the crosshair
//! on the doorway they care about, leaves it there for seconds, then moves to
//! the next one.
//!
//! ## The model
//!
//! Two states, and one interrupt:
//!
//! * **Dwell** — sit on a watch point for 1.2–3.4 s (drawn per bot, per look).
//! * **Shift** — pick the next watch point and hold that. The move itself is
//!   not animated here: the aim spring in [`crate::aim`] turns the head, which
//!   is what makes the motion look like a person rather than a lerp.
//! * **Sound** — anything heard loudly enough pulls the look toward it after a
//!   human reaction delay, holds it there for a beat, then the sweep resumes.
//!
//! The watch points come from the map, not from an arc: `nav::watch` returns
//! the places an enemy can walk in from *that are visible from here*.
//!
//! ## The one constraint that is not aesthetic
//!
//! `CheckActivityInGame` kicks a client whose view has not moved on **both**
//! axes between samples five seconds apart ([`crate::idle`]). A dwell is by
//! design a period of not moving, so dwells are capped below that window and
//! the small anti-idle drift stays applied underneath. [`Scan::dwell_is_safe`]
//! is the assertion, and the test sweeps every phase.

use crate::math::Vec3;
use crate::rng::Rng;

/// Longest a look may rest on one point.
///
/// Under [`crate::idle::IDLE_CHECK_INTERVAL`] with room to spare, so a
/// five-second idle window always contains at least one shift.
pub const DWELL_MAX: f32 = 3.4;
/// Shortest, so the crosshair never looks twitchy.
pub const DWELL_MIN: f32 = 1.2;

/// How long a bot takes to react to a sound before its head starts to move.
///
/// Human simple reaction time is around 200 ms; drawn per look so two bots
/// hearing the same shot do not turn in lockstep.
pub const REACT_MIN: f32 = 0.15;
pub const REACT_MAX: f32 = 0.38;

/// How long the head stays on a sound before the sweep resumes.
pub const INTEREST_MIN: f32 = 1.1;
pub const INTEREST_MAX: f32 = 2.6;

/// Below this urgency a sound is not worth turning for — distant, or stale.
///
/// It was 0.12, which on a live server is "anything at all": measured over a
/// ten-minute run, **every single sample of a bot holding a position had a
/// sound above the old threshold**, so the head was permanently on the last
/// noise and never on the angles the bot was supposed to be watching. A player
/// glances at nearby gunfire; they do not abandon their crosshair for every
/// shot on the map.
pub const HEAR_THRESHOLD: f32 = 0.38;

/// Least time between two sound glances.
///
/// Without it a firefight is a continuous stream of sounds each just louder
/// than the last, and the head never returns to the sweep at all.
pub const GLANCE_COOLDOWN: f32 = 2.2;

/// A sound the bot has decided to look at.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Interest {
    point: Vec3,
    /// Seconds before the head starts moving. Counts down first.
    delay: f32,
    /// Seconds to keep looking once the delay has passed.
    left: f32,
    /// What it scored when accepted, so a louder one can override it.
    urgency: f32,
}

/// The look behaviour of a bot holding a position.
#[derive(Debug, Clone)]
pub struct Scan {
    /// Points worth watching, in no particular order.
    watch: Vec<Vec3>,
    /// Which one is being looked at.
    at: usize,
    /// Seconds left on this dwell.
    dwell_left: f32,
    interest: Option<Interest>,
    /// Seconds until another sound may take the head.
    cooldown: f32,
    rng: Rng,
}

impl Scan {
    /// A scan belonging to this bot and no other.
    ///
    /// The dwell lengths and the order points are visited in are drawn from
    /// the bot's own seed: two bots holding the same doorway should not move
    /// their heads in step, which is the tell that gives a swarm away.
    pub fn from_seed(seed: u64) -> Self {
        Self {
            watch: Vec::new(),
            at: 0,
            dwell_left: 0.0,
            interest: None,
            cooldown: 0.0,
            rng: Rng::new(seed ^ 0x5CA1_AB1E),
        }
    }

    /// Replace the points being watched.
    ///
    /// Called when the bot starts a new hold. Keeps the current index in range
    /// rather than resetting, so re-supplying the same points mid-hold does
    /// not snap the head back to the first one.
    pub fn watch(&mut self, points: &[Vec3]) {
        // Only a change in HOW MANY points restarts the look. The caller
        // refreshes this every tick, and the points drift by a unit or two as
        // the defend point is re-picked; treating that as new would zero the
        // dwell on every tick, so the head would advance to the next target
        // every frame and never settle on any of them. Measured live, that
        // left a bot a median 45 degrees off the nearest of its own sight
        // lines -- permanently in transit between two of them.
        let resize = self.watch.len() != points.len();
        self.watch = points.to_vec();
        if self.watch.is_empty() {
            self.at = 0;
            return;
        }
        self.at %= self.watch.len();
        if resize {
            self.dwell_left = 0.0;
        }
    }

    pub fn watching(&self) -> &[Vec3] {
        &self.watch
    }

    /// True while the head is on a sound rather than on the sweep.
    pub fn distracted(&self) -> bool {
        self.interest.is_some_and(|i| i.delay <= 0.0)
    }

    /// Offer a sound. Louder-and-fresher wins; quiet ones are ignored.
    ///
    /// `urgency` is [`crate::world::Heard::urgency`]: loudness already
    /// discounted by age.
    pub fn hear(&mut self, point: Vec3, urgency: f32) {
        if urgency < HEAR_THRESHOLD || self.cooldown > 0.0 {
            return;
        }
        // Do not re-trigger on the same noise, but do let a louder one take
        // over: a footstep behind you stops mattering when a rifle opens up.
        if self.interest.is_some_and(|i| i.urgency >= urgency) {
            return;
        }
        let delay = self.rng.range(f64::from(REACT_MIN), f64::from(REACT_MAX)) as f32;
        let left = self.rng.range(f64::from(INTEREST_MIN), f64::from(INTEREST_MAX)) as f32;
        self.interest = Some(Interest {
            point,
            delay,
            left,
            urgency,
        });
    }

    /// Advance the model and return the point to look at, if there is one.
    ///
    /// `None` means "no opinion" — no watch points and nothing heard — and the
    /// caller should keep whatever it was looking at rather than snap
    /// somewhere arbitrary.
    pub fn advance(&mut self, dt: f32) -> Option<Vec3> {
        self.cooldown = (self.cooldown - dt).max(0.0);
        if let Some(interest) = self.interest.as_mut() {
            if interest.delay > 0.0 {
                interest.delay -= dt;
            } else {
                interest.left -= dt;
            }
            let expired = interest.left <= 0.0;
            let looking = interest.delay <= 0.0;
            let point = interest.point;
            if expired {
                self.interest = None;
                self.cooldown = GLANCE_COOLDOWN;
                // Resume the sweep on a fresh dwell rather than the remains of
                // the one interrupted.
                self.dwell_left = 0.0;
            } else if looking {
                return Some(point);
            }
        }

        if self.watch.is_empty() {
            return None;
        }
        self.dwell_left -= dt;
        if self.dwell_left <= 0.0 {
            self.at = self.next_index();
            self.dwell_left = self.rng.range(f64::from(DWELL_MIN), f64::from(DWELL_MAX)) as f32;
        }
        self.watch.get(self.at).copied()
    }

    /// Which point to look at next.
    ///
    /// Mostly the next one round, but one time in four it jumps to a random
    /// other point instead. A perfectly cyclic sweep is as much of a tell as a
    /// perfectly still one — real attention goes back to the angle it is
    /// worried about.
    fn next_index(&mut self) -> usize {
        let n = self.watch.len();
        if n <= 1 {
            return 0;
        }
        if self.rng.chance(0.25) {
            let skip = self.rng.range_i(1, n as i64 - 1) as usize;
            (self.at + skip) % n
        } else {
            (self.at + 1) % n
        }
    }

    /// Every dwell this model can draw is shorter than the idle window.
    ///
    /// Not a test helper: a dwell as long as `CheckActivityInGame`'s five
    /// seconds could put both of its samples inside one motionless hold, and
    /// the bot would be kicked for idling twenty minutes into a match.
    pub const fn dwell_is_safe() -> bool {
        DWELL_MAX < crate::idle::IDLE_CHECK_INTERVAL && DWELL_MIN > 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Vec3 = [100.0, 0.0, 64.0];
    const B: Vec3 = [0.0, 100.0, 64.0];
    const C: Vec3 = [-100.0, 0.0, 64.0];

    #[test]
    fn a_dwell_can_never_span_the_idle_window() {
        assert!(Scan::dwell_is_safe());
    }

    #[test]
    fn the_look_parks_on_a_point_for_seconds_rather_than_creeping() {
        let mut s = Scan::from_seed(7);
        s.watch(&[A, B, C]);
        let mut held = 0.0f32;
        let mut longest = 0.0f32;
        let mut last = s.advance(0.05).expect("a point");
        for _ in 0..400 {
            let now = s.advance(0.05).expect("a point");
            if now == last {
                held += 0.05;
                longest = longest.max(held);
            } else {
                held = 0.0;
            }
            last = now;
        }
        assert!(
            longest >= DWELL_MIN,
            "never held a point for a full dwell: {longest}"
        );
        assert!(
            longest <= DWELL_MAX + 0.1,
            "held longer than the idle window allows: {longest}"
        );
    }

    #[test]
    fn every_watch_point_gets_looked_at() {
        let mut s = Scan::from_seed(11);
        s.watch(&[A, B, C]);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            if let Some(p) = s.advance(0.05) {
                seen.insert(format!("{p:?}"));
            }
        }
        assert_eq!(seen.len(), 3, "a watch point was never visited");
    }

    #[test]
    fn a_sound_turns_the_head_after_a_human_delay_and_then_lets_go() {
        let mut s = Scan::from_seed(3);
        s.watch(&[A]);
        let shot: Vec3 = [0.0, -900.0, 64.0];
        s.hear(shot, 0.9);

        // Nothing moves during the reaction delay.
        assert_eq!(s.advance(0.05), Some(A), "turned before reacting");
        assert!(!s.distracted());

        let mut looked_at_shot = false;
        let mut elapsed = 0.0;
        for _ in 0..20 {
            elapsed += 0.05;
            if s.advance(0.05) == Some(shot) {
                looked_at_shot = true;
                break;
            }
        }
        assert!(looked_at_shot, "never looked at the sound");
        assert!(
            (REACT_MIN..=REACT_MAX + 0.1).contains(&elapsed),
            "reaction time {elapsed} is not human"
        );

        // And it goes back to watching afterwards.
        for _ in 0..200 {
            s.advance(0.05);
        }
        assert_eq!(s.advance(0.05), Some(A), "never let go of the sound");
    }

    #[test]
    fn a_quiet_sound_is_ignored_and_a_louder_one_takes_over() {
        let mut s = Scan::from_seed(5);
        s.watch(&[A]);
        s.hear(B, HEAR_THRESHOLD * 0.5);
        assert!(s.interest.is_none(), "turned toward something inaudible");

        s.hear(B, 0.5);
        let first = s.interest.expect("accepted the audible one").point;
        assert_eq!(first, B);
        s.hear(C, 0.45);
        assert_eq!(s.interest.expect("kept").point, B, "a quieter sound won");
        s.hear(C, 0.9);
        assert_eq!(s.interest.expect("kept").point, C, "a louder sound lost");
    }

    /// A firefight is a stream of sounds, each about as loud as the last. If
    /// every one of them could take the head, the crosshair would never come
    /// back to the angles being watched -- measured live with the old
    /// threshold, EVERY sample of a holding bot had a sound above it, so the
    /// sweep never ran at all.
    #[test]
    fn a_stream_of_gunfire_does_not_own_the_head_forever() {
        let mut s = Scan::from_seed(21);
        s.watch(&[A, B]);
        let shot: Vec3 = [0.0, -900.0, 64.0];

        let mut on_sound = 0;
        let mut on_sweep = 0;
        for tick in 0..400 {
            // A loud shot every quarter second, forever.
            if tick % 12 == 0 {
                s.hear(shot, 0.9);
            }
            match s.advance(0.05) {
                Some(p) if p == shot => on_sound += 1,
                Some(_) => on_sweep += 1,
                None => {}
            }
        }
        assert!(
            on_sweep > 0,
            "the sweep never ran: {on_sound} ticks on the sound, {on_sweep} on watch points"
        );
        assert!(
            on_sweep * 2 > on_sound,
            "under constant fire the head was on sounds {on_sound} ticks and on              its angles only {on_sweep}"
        );
    }

    #[test]
    fn with_nothing_to_watch_the_model_has_no_opinion() {
        let mut s = Scan::from_seed(1);
        assert_eq!(s.advance(0.1), None);
        // ...but a sound is still worth turning to.
        s.hear(A, 0.9);
        let mut turned = false;
        for _ in 0..20 {
            if s.advance(0.05) == Some(A) {
                turned = true;
            }
        }
        assert!(turned, "a sound should move the head even with no sweep");
    }

    #[test]
    fn two_bots_on_the_same_points_do_not_move_in_step() {
        let mut a = Scan::from_seed(1);
        let mut b = Scan::from_seed(2);
        a.watch(&[A, B, C]);
        b.watch(&[A, B, C]);
        let mut same = 0;
        let mut total = 0;
        for _ in 0..600 {
            let (pa, pb) = (a.advance(0.05), b.advance(0.05));
            total += 1;
            if pa == pb {
                same += 1;
            }
        }
        assert!(
            same * 3 < total * 2,
            "two seeds looked the same way {same}/{total} of the time"
        );
    }
}
