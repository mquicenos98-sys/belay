//! Monotonic integer counters.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// A monotonically increasing `u64`, e.g. jobs enqueued or requests served.
///
/// Clones share the same value. Updates are a single relaxed atomic add.
/// The value wraps at `u64::MAX`; at a billion increments per second that
/// takes over 500 years, so no saturation check is spent on the hot path.
///
/// ```
/// use belay_metrics::Counter;
///
/// let served = Counter::new();
/// let handle = served.clone();
/// handle.inc();
/// handle.inc_by(41);
/// assert_eq!(served.get(), 42);
/// ```
#[derive(Clone, Default)]
pub struct Counter {
    value: Arc<AtomicU64>,
}

impl Counter {
    /// A standalone counter at zero, not attached to any registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one.
    #[inline]
    pub fn inc(&self) {
        self.value.fetch_add(1, Ordering::Relaxed);
    }

    /// Adds `n`.
    #[inline]
    pub fn inc_by(&self, n: u64) {
        self.value.fetch_add(n, Ordering::Relaxed);
    }

    /// The current value.
    #[must_use]
    pub fn get(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }
}

impl fmt::Debug for Counter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Counter")
            .field("value", &self.get())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_the_value() {
        let a = Counter::new();
        let b = a.clone();
        a.inc();
        b.inc_by(10);
        assert_eq!(a.get(), 11);
        assert_eq!(format!("{b:?}"), "Counter { value: 11 }");
    }

    #[test]
    fn wraps_instead_of_panicking_at_the_limit() {
        let c = Counter::new();
        c.inc_by(u64::MAX);
        c.inc_by(2);
        assert_eq!(c.get(), 1);
    }
}
