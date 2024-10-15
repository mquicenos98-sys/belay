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
