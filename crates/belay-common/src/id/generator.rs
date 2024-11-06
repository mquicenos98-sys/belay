//! Monotonic ULID generation.
//!
//! Within one millisecond the ULID spec's monotonic mode increments the
//! random part of the previous ID instead of drawing new randomness, so IDs
//! created in quick succession still sort in creation order. The same rule
//! covers a clock that steps backwards: new IDs keep the last timestamp and
//! keep counting up, so IDs never decrease.

use std::fmt;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use super::{random_bits, timestamp_bits, Ulid};
use crate::rand::Rng;
use crate::sync::lock;
use crate::time::{system_clock, SharedClock, Timestamp};

/// How many times [`SyncUlidGenerator::generate`] waits a millisecond for
/// the clock to move on after exhausting a millisecond's ID space.
const OVERFLOW_RETRIES: u32 = 5;

/// A single-threaded monotonic ULID generator.
///
/// ```
/// use belay_common::id::UlidGenerator;
/// use belay_common::rand::Rng;
/// use belay_common::time::Timestamp;
///
/// let mut ids = UlidGenerator::with_rng(Rng::seed_from_u64(1));
/// let now = Timestamp::from_millis(1_700_000_000_000);
/// let a = ids.generate(now).unwrap();
/// let b = ids.generate(now).unwrap();
/// assert_eq!(b.to_u128(), a.to_u128() + 1);
/// ```
pub struct UlidGenerator {
    last: Option<Ulid>,
    rng: Rng,
}

impl UlidGenerator {
    /// A generator with entropy-seeded randomness.
    #[must_use]
    pub fn new() -> UlidGenerator {
        UlidGenerator::with_rng(Rng::from_entropy())
    }

    /// A generator drawing randomness from `rng`; with a seeded `rng` the
    /// output is reproducible.
    #[must_use]
    pub fn with_rng(rng: Rng) -> UlidGenerator {
        UlidGenerator { last: None, rng }
    }

    /// The next ULID for time `now`, strictly greater than the previous one.
    ///
    /// Fails only when the random part can no longer be incremented within
    /// the current millisecond (after about 2^79 IDs on average, since it
    /// starts at a random value); the caller can retry once the clock has
    /// advanced.
    pub fn generate(&mut self, now: Timestamp) -> Result<Ulid, UlidOverflow> {
        let ms = timestamp_bits(now);
        let id = match self.last {
            Some(last) if ms <= last.timestamp_ms() => last.increment().ok_or(UlidOverflow {
                timestamp_ms: last.timestamp_ms(),
            })?,
            _ => Ulid::from_parts(ms, random_bits(&mut self.rng)),
        };
        self.last = Some(id);
        Ok(id)
    }

    /// The ULID returned by the last successful [`generate`](Self::generate).
    #[must_use]
    pub fn last(&self) -> Option<Ulid> {
        self.last
    }
}
