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

impl Default for UlidGenerator {
    fn default() -> Self {
        UlidGenerator::new()
    }
}

impl fmt::Debug for UlidGenerator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UlidGenerator")
            .field("last", &self.last)
            .finish_non_exhaustive()
    }
}

/// A thread-safe monotonic generator that reads time from a clock.
///
/// ```
/// use belay_common::id::SyncUlidGenerator;
/// use belay_common::time::{ManualClock, Timestamp};
///
/// let clock = ManualClock::new(Timestamp::from_secs(1_700_000_000));
/// let ids = SyncUlidGenerator::new(clock.shared());
/// let id = ids.generate().unwrap();
/// assert_eq!(id.timestamp(), Timestamp::from_secs(1_700_000_000));
/// ```
pub struct SyncUlidGenerator {
    inner: Mutex<UlidGenerator>,
    clock: SharedClock,
}

impl SyncUlidGenerator {
    /// A generator with entropy-seeded randomness.
    #[must_use]
    pub fn new(clock: SharedClock) -> SyncUlidGenerator {
        SyncUlidGenerator::with_rng(clock, Rng::from_entropy())
    }

    /// A generator drawing randomness from `rng`.
    #[must_use]
    pub fn with_rng(clock: SharedClock, rng: Rng) -> SyncUlidGenerator {
        SyncUlidGenerator {
            inner: Mutex::new(UlidGenerator::with_rng(rng)),
            clock,
        }
    }

    /// The next ULID. If the current millisecond's ID space is exhausted,
    /// sleeps a millisecond at a time (a few times at most) waiting for the
    /// clock to advance, then gives up with [`UlidOverflow`].
    pub fn generate(&self) -> Result<Ulid, UlidOverflow> {
        let mut attempt = 0;
        loop {
            let result = lock(&self.inner).generate(self.clock.now());
            match result {
                Err(_) if attempt < OVERFLOW_RETRIES => {
                    attempt += 1;
                    thread::sleep(Duration::from_millis(1));
                }
                other => return other,
            }
        }
    }
}

impl fmt::Debug for SyncUlidGenerator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncUlidGenerator")
            .field("last", &lock(&self.inner).last())
            .finish_non_exhaustive()
    }
}

/// Backs [`Ulid::generate`].
pub(super) fn global() -> Ulid {
    static GLOBAL: OnceLock<SyncUlidGenerator> = OnceLock::new();
    let ids = GLOBAL.get_or_init(|| SyncUlidGenerator::new(system_clock()));
    loop {
        // Overflow needs ~2^80 IDs in one millisecond; `generate` has
        // already slept, and the system clock always moves on eventually.
        if let Ok(id) = ids.generate() {
            return id;
        }
    }
}

/// The random part of a millisecond's IDs has been used up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UlidOverflow {
    /// The millisecond whose ID space is exhausted.
    pub timestamp_ms: u64,
}

impl fmt::Display for UlidOverflow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ULID space exhausted for millisecond {}; retry after the clock advances",
            self.timestamp_ms
        )
    }
}

impl std::error::Error for UlidOverflow {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::RANDOM_MASK;
    use crate::time::ManualClock;
    use std::collections::HashSet;
    use std::sync::Arc;

    const T0: i64 = 1_700_000_000_000;

    fn seeded() -> UlidGenerator {
        UlidGenerator::with_rng(Rng::seed_from_u64(99))
    }

    #[test]
    fn same_millisecond_increments_the_random_part() {
        let mut g = seeded();
        let now = Timestamp::from_millis(T0);
        let a = g.generate(now).expect("fresh generator");
        let b = g.generate(now).expect("same ms");
        let c = g.generate(now).expect("same ms");
        assert_eq!(b.to_u128(), a.to_u128() + 1);
        assert_eq!(c.to_u128(), b.to_u128() + 1);
        assert_eq!(c.timestamp_ms(), T0 as u64);
        assert_eq!(g.last(), Some(c));
    }

    #[test]
    fn new_millisecond_draws_fresh_randomness() {
        let mut g = seeded();
        let a = g.generate(Timestamp::from_millis(T0)).expect("ok");
        let b = g.generate(Timestamp::from_millis(T0 + 1)).expect("ok");
        assert!(b > a);
        assert_eq!(b.timestamp_ms(), T0 as u64 + 1);
        assert_ne!(b.random(), a.random() + 1);
    }

    #[test]
    fn clock_going_backwards_keeps_ids_increasing() {
        let mut g = seeded();
        let a = g.generate(Timestamp::from_millis(T0)).expect("ok");
        let b = g.generate(Timestamp::from_millis(T0 - 5_000)).expect("ok");
        assert!(b > a);
        assert_eq!(b.timestamp_ms(), a.timestamp_ms());
    }

    #[test]
    fn exhausted_millisecond_reports_overflow_until_time_moves() {
        let mut g = seeded();
        g.last = Some(Ulid::from_parts(T0 as u64, RANDOM_MASK - 1));
        let now = Timestamp::from_millis(T0);
        assert!(g.generate(now).is_ok());
        let err = g.generate(now).unwrap_err();
        assert_eq!(err.timestamp_ms, T0 as u64);
        assert_eq!(
            err.to_string(),
            "ULID space exhausted for millisecond 1700000000000; retry after the clock advances"
        );
        let next = g.generate(Timestamp::from_millis(T0 + 1)).expect("next ms");
        assert_eq!(next.timestamp_ms(), T0 as u64 + 1);
    }

    #[test]
    fn first_id_at_the_epoch_is_random_not_a_counter() {
        let mut g = seeded();
        let id = g.generate(Timestamp::EPOCH).expect("ok");
        assert_eq!(id.timestamp_ms(), 0);
        assert!(id.random() > 1, "random part should not start at 0/1");
    }

    #[test]
    fn seeded_generators_are_reproducible() {
        let mut a = seeded();
        let mut b = seeded();
        for i in 0..10 {
            let now = Timestamp::from_millis(T0 + i / 3);
            assert_eq!(a.generate(now), b.generate(now));
        }
    }

    #[test]
    fn sync_generator_is_unique_and_ordered_per_thread_across_threads() {
        let clock = ManualClock::new(Timestamp::from_millis(T0));
        let ids = Arc::new(SyncUlidGenerator::new(clock.shared()));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let ids = Arc::clone(&ids);
                let clock = clock.clone();
                thread::spawn(move || {
                    let mut mine = Vec::new();
                    for i in 0..500 {
                        if i % 100 == 0 {
                            clock.advance(Duration::from_millis(1));
                        }
                        mine.push(ids.generate().expect("no overflow"));
                    }
                    mine
                })
            })
            .collect();
        let mut all = HashSet::new();
        for h in handles {
            let mine = h.join().expect("generator thread panicked");
            assert!(mine.windows(2).all(|w| w[0] < w[1]));
            all.extend(mine);
        }
        assert_eq!(all.len(), 2_000);
    }

    #[test]
    fn sync_generator_gives_up_when_the_clock_is_stuck() {
        let clock = ManualClock::new(Timestamp::from_millis(T0));
        let ids = SyncUlidGenerator::new(clock.shared());
        lock(&ids.inner).last = Some(Ulid::max_at(Timestamp::from_millis(T0)));
        assert!(ids.generate().is_err());
        clock.advance(Duration::from_millis(1));
        assert!(ids.generate().is_ok());
    }

    #[test]
    fn global_generate_is_monotonic() {
        let ids: Vec<Ulid> = (0..1_000).map(|_| Ulid::generate()).collect();
        assert!(ids.windows(2).all(|w| w[0] < w[1]));
        let now = Timestamp::now().as_millis() as u64;
        assert!(ids[999].timestamp_ms() <= now);
        assert!(now - ids[0].timestamp_ms() < 60_000);
    }
}
