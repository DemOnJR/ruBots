//! Randomness for bot behaviour.
//!
//! Port of `rnd` and `chance` from `internal/bot/util.go` (`0x140703E60`,
//! `0x140703F20`). The original defers to Go's `math/rand`; the exact
//! generator is not part of the observable behaviour, so this uses a small
//! deterministic PRNG instead — which has the advantage of making bot
//! behaviour reproducible in tests.
//!
//! `chance` is called throughout `engage`, `chooseTask` and `doBuy` to keep
//! bots from behaving identically.

/// xorshift64* — small, fast, and good enough for behavioural jitter.
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        // Avoid the zero state, which xorshift cannot escape.
        Self { state: if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed } }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        // Top 53 bits give an exact double in [0,1).
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in `[min, max)` — `rnd` in the original.
    pub fn range(&mut self, min: f64, max: f64) -> f64 {
        if max <= min {
            return min;
        }
        min + self.unit() * (max - min)
    }

    /// Uniform integer in `[min, max]`.
    pub fn range_i(&mut self, min: i64, max: i64) -> i64 {
        if max <= min {
            return min;
        }
        min + (self.next_u64() % ((max - min + 1) as u64)) as i64
    }

    /// True with probability `p` — `chance` in the original.
    pub fn chance(&mut self, p: f64) -> bool {
        if p <= 0.0 {
            return false;
        }
        if p >= 1.0 {
            return true;
        }
        self.unit() < p
    }

    /// Pick an element at random.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() {
            return None;
        }
        let i = (self.next_u64() % items.len() as u64) as usize;
        items.get(i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_stays_in_range() {
        let mut r = Rng::new(7);
        for _ in 0..10_000 {
            let v = r.unit();
            assert!((0.0..1.0).contains(&v), "{v} out of range");
        }
    }

    #[test]
    fn certain_and_impossible_chances_are_absolute() {
        let mut r = Rng::new(3);
        for _ in 0..100 {
            assert!(r.chance(1.0));
            assert!(!r.chance(0.0));
            assert!(!r.chance(-1.0));
            assert!(r.chance(2.0));
        }
    }

    #[test]
    fn chance_is_roughly_calibrated() {
        let mut r = Rng::new(99);
        let hits = (0..20_000).filter(|_| r.chance(0.25)).count();
        assert!(
            (4200..5800).contains(&hits),
            "expected ~5000 of 20000, got {hits}"
        );
    }

    #[test]
    fn range_respects_bounds_and_degenerate_input() {
        let mut r = Rng::new(11);
        for _ in 0..1000 {
            let v = r.range(-5.0, 5.0);
            assert!((-5.0..5.0).contains(&v));
        }
        assert_eq!(r.range(3.0, 3.0), 3.0);
        assert_eq!(r.range(9.0, 1.0), 9.0, "inverted bounds must not panic");
    }

    #[test]
    fn integer_range_is_inclusive_and_covers_its_span() {
        let mut r = Rng::new(5);
        let mut seen = [false; 6];
        for _ in 0..2000 {
            let v = r.range_i(0, 5);
            assert!((0..=5).contains(&v));
            seen[v as usize] = true;
        }
        assert!(seen.iter().all(|s| *s), "every value should occur");
        assert_eq!(r.range_i(4, 4), 4);
    }

    #[test]
    fn same_seed_gives_the_same_stream() {
        let mut a = Rng::new(2024);
        let mut b = Rng::new(2024);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn a_zero_seed_still_produces_variety() {
        let mut r = Rng::new(0);
        let first = r.next_u64();
        assert_ne!(first, 0);
        assert_ne!(first, r.next_u64());
    }

    #[test]
    fn pick_handles_the_empty_case() {
        let mut r = Rng::new(1);
        let empty: [u8; 0] = [];
        assert!(r.pick(&empty).is_none());
        assert_eq!(r.pick(&[42u8]), Some(&42));
    }
}
