//! A small, fast, seedable pseudo-random number generator.
//!
//! [`Rng`] is xoshiro256** (Blackman & Vigna, <https://prng.di.unimi.it/>),
//! seeded through SplitMix64 so that similar seeds still give unrelated
//! streams. It is *not* cryptographically secure: it serves retry jitter,
//! sampling, shuffling, ULID randomness and test data, none of which are
//! secrets.
//!
//! Tests should use [`Rng::seed_from_u64`] for reproducibility; production
//! code can call [`thread_rng`] for a cheap independently seeded generator.

use std::cell::RefCell;
use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::{BuildHasher, Hash, Hasher};
use std::ops::{Range, RangeInclusive};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime};

/// The SplitMix64 increment (2^64 / golden ratio, forced odd).
const GOLDEN_GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

/// SplitMix64 (Steele, Lea & Flood), used only to expand seeds.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(GOLDEN_GAMMA);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// xoshiro256** pseudo-random number generator.
///
/// ```
/// use belay_common::rand::Rng;
///
/// let mut a = Rng::seed_from_u64(42);
/// let mut b = Rng::seed_from_u64(42);
/// assert_eq!(a.next_u64(), b.next_u64());
///
/// let roll = a.gen_range(1..=6);
/// assert!((1..=6).contains(&roll));
/// let index = a.gen_range(0..10usize);
/// assert!(index < 10);
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct Rng {
    s: [u64; 4],
}

impl Rng {
    /// A generator whose whole output is determined by `seed`.
    #[must_use]
    pub fn seed_from_u64(seed: u64) -> Rng {
        let mut sm = SplitMix64(seed);
        // SplitMix64 outputs are distinct for distinct counters, so the
        // state can never be all zeros (the one invalid xoshiro state).
        Rng {
            s: [sm.next(), sm.next(), sm.next(), sm.next()],
        }
    }

    /// A generator seeded from process-local entropy: the wall clock, a
    /// monotonic instant, `RandomState` (whose keys come from the OS random
    /// source), the thread id, the process id, a stack address and a global
    /// counter. Two calls never return the same stream in practice.
    #[must_use]
    pub fn from_entropy() -> Rng {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let local = 0u8;
        let digest = |salt: u64| {
            let mut h = RandomState::new().build_hasher();
            salt.hash(&mut h);
            count.hash(&mut h);
            SystemTime::now().hash(&mut h);
            Instant::now().hash(&mut h);
            std::thread::current().id().hash(&mut h);
            std::process::id().hash(&mut h);
            (std::ptr::addr_of!(local) as usize).hash(&mut h);
            h.finish()
        };
        let mut a = SplitMix64(digest(1));
        let mut b = SplitMix64(digest(2));
        // s[0] != s[2] because they are consecutive outputs of `a`.
        Rng {
            s: [a.next(), b.next(), a.next(), b.next()],
        }
    }

    /// A new generator seeded from this one's output, for handing an
    /// independent stream to another component or thread.
    #[must_use]
    pub fn fork(&mut self) -> Rng {
        Rng::seed_from_u64(self.next_u64())
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        let s = &mut self.s;
        let result = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        result
    }

    /// The next 32 random bits (the high half of [`next_u64`](Self::next_u64),
    /// which has the better statistical quality).
    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    /// A uniformly distributed `f64` in `[0, 1)` with 53 bits of precision.
    pub fn gen_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// `true` with probability `p`. Values of `p` at or below 0 (and NaN)
    /// always give `false`; values at or above 1 always give `true`.
    pub fn gen_bool(&mut self, p: f64) -> bool {
        if p.is_nan() || p <= 0.0 {
            false
        } else if p >= 1.0 {
            true
        } else {
            self.gen_f64() < p
        }
    }

    /// A uniformly distributed value in `range`, which may be half-open
    /// (`a..b`) or inclusive (`a..=b`) over any primitive integer type, or a
    /// half-open `f64` range.
    ///
    /// # Panics
    ///
    /// Panics if the range is empty, like slice indexing out of bounds; an
    /// empty range is a bug in the caller rather than a runtime condition.
    pub fn gen_range<T, R: SampleRange<T>>(&mut self, range: R) -> T {
        range.sample(self)
    }

    /// Uniform in `[0, n)` without modulo bias (Lemire's multiply-shift with
    /// rejection). `n` must be non-zero.
    fn below(&mut self, n: u64) -> u64 {
        debug_assert!(n > 0);
        let mut m = u128::from(self.next_u64()) * u128::from(n);
        if (m as u64) < n {
            let threshold = n.wrapping_neg() % n;
            while (m as u64) < threshold {
                m = u128::from(self.next_u64()) * u128::from(n);
            }
        }
        (m >> 64) as u64
    }

    /// Shuffles `items` in place (Fisher-Yates); every permutation is
    /// equally likely.
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            items.swap(i, j);
        }
    }

    /// A uniformly chosen element, or `None` if `items` is empty.
    pub fn choose<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() {
            None
        } else {
            items.get(self.below(items.len() as u64) as usize)
        }
    }

    /// Fills `buf` with random bytes.
    pub fn fill_bytes(&mut self, buf: &mut [u8]) {
        let mut chunks = buf.chunks_exact_mut(8);
        for chunk in &mut chunks {
            chunk.copy_from_slice(&self.next_u64().to_le_bytes());
        }
        let rest = chunks.into_remainder();
        if !rest.is_empty() {
            let bytes = self.next_u64().to_le_bytes();
            rest.copy_from_slice(&bytes[..rest.len()]);
        }
    }
}

/// The internal state is deliberately not printed.
impl fmt::Debug for Rng {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Rng { .. }")
    }
}

/// A range that [`Rng::gen_range`] can sample from.
pub trait SampleRange<T> {
    /// Draws one uniformly distributed value.
    fn sample(self, rng: &mut Rng) -> T;
}

macro_rules! int_sample_range {
    ($($t:ty => $unsigned:ty),* $(,)?) => {$(
        impl SampleRange<$t> for Range<$t> {
            fn sample(self, rng: &mut Rng) -> $t {
                assert!(
                    self.start < self.end,
                    "cannot sample from empty range {}..{}",
                    self.start,
                    self.end
                );
                // Two's-complement wrapping makes the span and the offset
                // correct for signed types too.
                let span = self.end.wrapping_sub(self.start) as $unsigned as u64;
                self.start.wrapping_add(rng.below(span) as $t)
            }
        }

        impl SampleRange<$t> for RangeInclusive<$t> {
            fn sample(self, rng: &mut Rng) -> $t {
                let (start, end) = self.into_inner();
                assert!(start <= end, "cannot sample from empty range {start}..={end}");
                let span = (end.wrapping_sub(start) as $unsigned as u64).wrapping_add(1);
                // A zero span means the full 64-bit domain.
                let offset = if span == 0 { rng.next_u64() } else { rng.below(span) };
                start.wrapping_add(offset as $t)
            }
        }
    )*};
}

int_sample_range!(
    u8 => u8, u16 => u16, u32 => u32, u64 => u64, usize => usize,
    i8 => u8, i16 => u16, i32 => u32, i64 => u64, isize => usize,
);

impl SampleRange<f64> for Range<f64> {
    fn sample(self, rng: &mut Rng) -> f64 {
        assert!(
            self.start < self.end && (self.end - self.start).is_finite(),
            "cannot sample from range {}..{}",
            self.start,
            self.end
        );
        loop {
            let v = self.start + (self.end - self.start) * rng.gen_f64();
            // Rounding can land exactly on `end`; redraw to keep it half-open.
            if v < self.end {
                return v;
            }
        }
    }
}

thread_local! {
    static THREAD_RNG: RefCell<Rng> = RefCell::new(Rng::from_entropy());
}

/// A fresh generator forked from a per-thread entropy-seeded stream.
///
/// Cheap enough to call wherever randomness is needed; the returned `Rng` is
/// independent of every other one handed out.
///
/// ```
/// use belay_common::rand::thread_rng;
///
/// let mut rng = thread_rng();
/// let mut ids = [1, 2, 3, 4];
/// rng.shuffle(&mut ids);
/// ```
#[must_use]
pub fn thread_rng() -> Rng {
    THREAD_RNG
        .try_with(|rng| rng.borrow_mut().fork())
        // Only during thread-local destruction.
        .unwrap_or_else(|_| Rng::from_entropy())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix64_matches_reference_output() {
        let mut sm = SplitMix64(0);
        assert_eq!(sm.next(), 0xE220_A839_7B1D_CDAF);
    }

    #[test]
    fn xoshiro256starstar_matches_reference_output() {
        let mut rng = Rng { s: [1, 2, 3, 4] };
        let expected = [11_520, 0, 1_509_978_240, 1_215_971_899_390_074_240];
        for e in expected {
            assert_eq!(rng.next_u64(), e);
        }
    }

    #[test]
    fn same_seed_same_stream_different_seed_different_stream() {
        let mut a = Rng::seed_from_u64(1);
        let mut b = Rng::seed_from_u64(1);
        let mut c = Rng::seed_from_u64(2);
        let xs: Vec<u64> = (0..16).map(|_| a.next_u64()).collect();
        let ys: Vec<u64> = (0..16).map(|_| b.next_u64()).collect();
        let zs: Vec<u64> = (0..16).map(|_| c.next_u64()).collect();
        assert_eq!(xs, ys);
        assert_ne!(xs, zs);
    }

    #[test]
    fn entropy_and_thread_generators_differ() {
        let mut a = Rng::from_entropy();
        let mut b = Rng::from_entropy();
        assert_ne!(a.next_u64(), b.next_u64());
        let mut c = thread_rng();
        let mut d = thread_rng();
        assert_ne!(c.next_u64(), d.next_u64());
        let from_other_thread = std::thread::spawn(|| thread_rng().next_u64())
            .join()
            .expect("thread panicked");
        assert_ne!(from_other_thread, thread_rng().next_u64());
    }

    #[test]
    fn gen_range_stays_in_bounds_and_hits_both_ends() {
        let mut rng = Rng::seed_from_u64(3);
        let mut seen = [false; 5];
        for _ in 0..1_000 {
            let v = rng.gen_range(10..15u64);
            assert!((10..15).contains(&v));
            seen[(v - 10) as usize] = true;
        }
        assert!(seen.iter().all(|&s| s));

        let mut lo = false;
        let mut hi = false;
        for _ in 0..1_000 {
            let v: i32 = rng.gen_range(-3..=3);
            assert!((-3..=3).contains(&v));
            lo |= v == -3;
            hi |= v == 3;
        }
        assert!(lo && hi);
    }

    #[test]
    fn gen_range_handles_extreme_bounds() {
        let mut rng = Rng::seed_from_u64(4);
        for _ in 0..100 {
            let _: u64 = rng.gen_range(0..=u64::MAX);
            let v: i64 = rng.gen_range(i64::MIN..i64::MAX);
            assert!(v < i64::MAX);
            let b: u8 = rng.gen_range(0..=255);
            let _ = b;
            let s: i8 = rng.gen_range(-128..=127);
            let _ = s;
        }
        assert_eq!(rng.gen_range(7..8u32), 7);
        assert_eq!(rng.gen_range(9..=9usize), 9);
    }

    #[test]
    #[should_panic(expected = "cannot sample from empty range 5..5")]
    fn gen_range_panics_on_empty_range() {
        Rng::seed_from_u64(0).gen_range(5..5u32);
    }

    #[test]
    fn gen_range_is_roughly_uniform() {
        // 60k draws over 3 buckets: each should be within 3% of 20k. The
        // seed is fixed, so this is deterministic, not flaky.
        let mut rng = Rng::seed_from_u64(5);
        let mut counts = [0u32; 3];
        for _ in 0..60_000 {
            counts[rng.gen_range(0..3usize)] += 1;
        }
        for c in counts {
            assert!((19_400..=20_600).contains(&c), "{counts:?}");
        }
    }

    #[test]
    fn float_range_is_half_open() {
        let mut rng = Rng::seed_from_u64(6);
        for _ in 0..1_000 {
            let v = rng.gen_range(-1.5..2.5);
            assert!((-1.5..2.5).contains(&v));
            let u = rng.gen_f64();
            assert!((0.0..1.0).contains(&u));
        }
    }

    #[test]
    fn gen_bool_edge_probabilities() {
        let mut rng = Rng::seed_from_u64(7);
        for _ in 0..100 {
            assert!(!rng.gen_bool(0.0));
            assert!(!rng.gen_bool(-1.0));
            assert!(!rng.gen_bool(f64::NAN));
            assert!(rng.gen_bool(1.0));
            assert!(rng.gen_bool(2.0));
        }
        let hits = (0..10_000).filter(|_| rng.gen_bool(0.25)).count();
        assert!((2_300..=2_700).contains(&hits), "{hits}");
    }

    #[test]
    fn shuffle_permutes_and_choose_picks_members() {
        let mut rng = Rng::seed_from_u64(8);
        let mut v: Vec<u32> = (0..50).collect();
        rng.shuffle(&mut v);
        assert_ne!(v, (0..50).collect::<Vec<_>>());
        let mut sorted = v.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..50).collect::<Vec<_>>());

        let empty: [u8; 0] = [];
        assert_eq!(rng.choose(&empty), None);
        let one = [42];
        assert_eq!(rng.choose(&one), Some(&42));
        let picked = rng.choose(&v).copied();
        assert!(picked.is_some_and(|p| p < 50));

        let mut single = [1];
        rng.shuffle(&mut single);
        rng.shuffle(&mut [] as &mut [u8]);
    }

    #[test]
    fn fill_bytes_covers_partial_words_deterministically() {
        let mut a = Rng::seed_from_u64(9);
        let mut b = Rng::seed_from_u64(9);
        let mut x = [0u8; 13];
        let mut y = [0u8; 13];
        a.fill_bytes(&mut x);
        b.fill_bytes(&mut y);
        assert_eq!(x, y);
        assert!(x[8..].iter().any(|&byte| byte != 0));
    }

    #[test]
    fn fork_yields_an_independent_stream() {
        let mut parent = Rng::seed_from_u64(10);
        let mut child = parent.fork();
        assert_ne!(parent.next_u64(), child.next_u64());
        assert_eq!(format!("{child:?}"), "Rng { .. }");
    }
}
