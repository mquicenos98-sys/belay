//! Injectable time sources.
//!
//! Everything in belay that makes decisions based on the current time (lease
//! expiry, cron fires, retention) asks a [`Clock`] rather than the operating
//! system, so tests can move time explicitly with a [`ManualClock`].

use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use super::Timestamp;

/// A source of the current wall-clock time.
pub trait Clock: Send + Sync {
    /// The current instant.
    fn now(&self) -> Timestamp;
}

/// A clock shared between components.
pub type SharedClock = Arc<dyn Clock>;

/// Lets structs holding a [`SharedClock`] derive `Debug`.
impl fmt::Debug for dyn Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Clock").field("now", &self.now()).finish()
    }
}

impl<C: Clock + ?Sized> Clock for Arc<C> {
    fn now(&self) -> Timestamp {
        (**self).now()
    }
}

impl<C: Clock + ?Sized> Clock for Box<C> {
    fn now(&self) -> Timestamp {
        (**self).now()
    }
}

impl<C: Clock + ?Sized> Clock for &C {
    fn now(&self) -> Timestamp {
        (**self).now()
    }
}

/// The operating system's real-time clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

/// A process-wide shared [`SystemClock`].
#[must_use]
pub fn system_clock() -> SharedClock {
    static CLOCK: OnceLock<SharedClock> = OnceLock::new();
    Arc::clone(CLOCK.get_or_init(|| Arc::new(SystemClock)))
}

/// A clock that only moves when told to. Clones share the same time, so a
/// test can keep one handle and give another to the code under test.
///
/// ```
/// use belay_common::time::{Clock, ManualClock, Timestamp};
/// use std::time::Duration;
///
/// let clock = ManualClock::new(Timestamp::from_secs(1_000));
/// let shared = clock.shared();
/// clock.advance(Duration::from_secs(30));
/// assert_eq!(shared.now(), Timestamp::from_secs(1_030));
/// ```
#[derive(Clone, Default)]
pub struct ManualClock {
    millis: Arc<AtomicI64>,
}

impl ManualClock {
    /// A clock stopped at `start`.
    #[must_use]
    pub fn new(start: Timestamp) -> ManualClock {
        ManualClock {
            millis: Arc::new(AtomicI64::new(start.as_millis())),
        }
    }

    /// Jumps to `t`. Going backwards is allowed, which is useful for testing
    /// how code copes with clock adjustments.
    pub fn set(&self, t: Timestamp) {
        self.millis.store(t.as_millis(), Ordering::SeqCst);
    }

    /// Moves forward by `d` (saturating at [`Timestamp::MAX`]) and returns
    /// the new time.
    pub fn advance(&self, d: Duration) -> Timestamp {
        let mut current = self.millis.load(Ordering::SeqCst);
        loop {
            let next = Timestamp::from_millis(current).saturating_add(d);
            match self.millis.compare_exchange_weak(
                current,
                next.as_millis(),
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return next,
                Err(actual) => current = actual,
            }
        }
    }

    /// Wraps a clone of this clock as a [`SharedClock`].
    #[must_use]
    pub fn shared(&self) -> SharedClock {
        Arc::new(self.clone())
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(self.millis.load(Ordering::SeqCst))
    }
}

impl fmt::Debug for ManualClock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ManualClock({})", self.now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn manual_clock_clones_share_state() {
        let clock = ManualClock::new(Timestamp::from_secs(10));
        let other = clock.clone();
        other.set(Timestamp::from_secs(20));
        assert_eq!(clock.now(), Timestamp::from_secs(20));
        assert_eq!(
            clock.advance(Duration::from_millis(1_500)),
            Timestamp::from_millis(21_500)
        );
        assert_eq!(other.now(), Timestamp::from_millis(21_500));
    }

    #[test]
    fn manual_clock_can_go_backwards_and_saturates_forwards() {
        let clock = ManualClock::new(Timestamp::from_secs(100));
        clock.set(Timestamp::from_secs(50));
        assert_eq!(clock.now(), Timestamp::from_secs(50));
        assert_eq!(clock.advance(Duration::MAX), Timestamp::MAX);
    }

    #[test]
    fn concurrent_advances_are_not_lost() {
        let clock = ManualClock::new(Timestamp::EPOCH);
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let c = clock.clone();
                thread::spawn(move || {
                    for _ in 0..1_000 {
                        c.advance(Duration::from_millis(1));
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("advancing thread panicked");
        }
        assert_eq!(clock.now(), Timestamp::from_millis(4_000));
    }

    #[test]
    fn shared_clock_is_debuggable_and_usable_through_wrappers() {
        let clock = ManualClock::new(Timestamp::from_secs(0));
        let shared: SharedClock = clock.shared();
        assert_eq!(
            format!("{shared:?}"),
            "Clock { now: Timestamp(1970-01-01T00:00:00.000Z) }"
        );
        let boxed: Box<dyn Clock> = Box::new(clock.clone());
        assert_eq!(boxed.now(), Timestamp::EPOCH);
        fn now_of<C: Clock>(c: C) -> Timestamp {
            c.now()
        }
        assert_eq!(now_of(&clock), Timestamp::EPOCH);
        assert_eq!(
            format!("{clock:?}"),
            "ManualClock(1970-01-01T00:00:00.000Z)"
        );
    }

    #[test]
    fn system_clock_is_close_to_timestamp_now() {
        let a = Timestamp::now();
        let b = system_clock().now();
        let c = Timestamp::now();
        assert!(a <= b && b <= c);
    }
}
