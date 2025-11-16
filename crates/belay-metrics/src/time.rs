//! Monotonic time sources used by timers, summary windows and the process
//! collector's uptime.
//!
//! Metrics only ever need elapsed time, never the wall clock, so the trait is
//! a single "how long since some fixed origin" method. Tests inject
//! [`ManualTime`] to make timer readings and summary windows deterministic.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// A source of monotonic time.
pub trait TimeSource: Send + Sync + fmt::Debug {
    /// Time elapsed since an arbitrary origin that is fixed for the lifetime
    /// of the source. Successive calls should never go backwards; consumers
    /// clamp rather than panic if one does.
    fn now(&self) -> Duration;
}

/// A shareable, type-erased [`TimeSource`].
pub type SharedTimeSource = Arc<dyn TimeSource>;

/// Real time measured with [`Instant`] from the moment of construction.
#[derive(Debug, Clone, Copy)]
pub struct MonotonicTime {
    origin: Instant,
}

impl MonotonicTime {
    /// Starts measuring from now.
    pub fn new() -> Self {
        MonotonicTime {
            origin: Instant::now(),
        }
    }
}

impl Default for MonotonicTime {
    fn default() -> Self {
        Self::new()
    }
}

impl TimeSource for MonotonicTime {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// The process-wide [`MonotonicTime`] used when no source is injected.
pub(crate) fn monotonic() -> SharedTimeSource {
    static SHARED: OnceLock<SharedTimeSource> = OnceLock::new();
    Arc::clone(SHARED.get_or_init(|| Arc::new(MonotonicTime::new())))
}

/// A hand-driven clock for tests. Clones share the same reading.
///
/// ```
/// use belay_metrics::{ManualTime, TimeSource};
/// use std::time::Duration;
///
/// let time = ManualTime::new();
/// let view = time.clone();
/// time.advance(Duration::from_millis(250));
/// assert_eq!(view.now(), Duration::from_millis(250));
/// ```
#[derive(Debug, Clone, Default)]
pub struct ManualTime {
    nanos: Arc<AtomicU64>,
}

impl ManualTime {
    /// A clock reading zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// A clock reading `start`.
    pub fn starting_at(start: Duration) -> Self {
        let time = Self::new();
        time.set(start);
        time
    }

    /// Moves the clock forward by `by`, saturating at `u64::MAX` nanoseconds.
    pub fn advance(&self, by: Duration) {
        let by = duration_to_nanos(by);
        let mut current = self.nanos.load(Ordering::Relaxed);
        loop {
            let next = current.saturating_add(by);
            match self.nanos.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    /// Sets the reading. Going backwards is allowed so tests can probe how
    /// consumers cope with it.
    pub fn set(&self, to: Duration) {
        self.nanos.store(duration_to_nanos(to), Ordering::Relaxed);
    }
}

impl TimeSource for ManualTime {
    fn now(&self) -> Duration {
        Duration::from_nanos(self.nanos.load(Ordering::Relaxed))
    }
}

fn duration_to_nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_time_does_not_go_backwards() {
        let time = MonotonicTime::new();
        let a = time.now();
        let b = time.now();
        assert!(b >= a);
    }

    #[test]
    fn shared_monotonic_source_is_a_singleton() {
        let a = monotonic();
        let b = monotonic();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn manual_time_advances_and_saturates() {
        let time = ManualTime::starting_at(Duration::from_secs(5));
        time.advance(Duration::from_millis(1500));
        assert_eq!(time.now(), Duration::from_millis(6500));
        time.advance(Duration::MAX);
        assert_eq!(time.now(), Duration::from_nanos(u64::MAX));
        time.set(Duration::from_secs(1));
        assert_eq!(time.now(), Duration::from_secs(1));
    }
}
