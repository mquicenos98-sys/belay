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
