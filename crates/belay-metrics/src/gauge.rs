//! Gauges: `f64` values that go up and down.

use std::fmt;
use std::sync::Arc;

use crate::atomic::AtomicF64;

/// A value that can go up and down, e.g. queue depth or bytes in use.
///
/// Stored as `f64` bits in an `AtomicU64`: `set` is a plain store, the
/// arithmetic methods are a compare-and-swap loop. Integer values up to 2^53
/// are exact, so a gauge used as an up/down counter never drifts.
///
/// ```
/// use belay_metrics::Gauge;
///
/// let depth = Gauge::new();
/// depth.set(10.0);
/// depth.inc();
/// depth.sub(3.5);
/// assert_eq!(depth.get(), 7.5);
/// ```
#[derive(Clone, Default)]
pub struct Gauge {
    value: Arc<AtomicF64>,
}

impl Gauge {
    /// A standalone gauge at zero, not attached to any registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the value.
    #[inline]
    pub fn set(&self, value: f64) {
        self.value.store(value);
    }

    /// Adds one.
    #[inline]
    pub fn inc(&self) {
        self.value.add(1.0);
    }

    /// Subtracts one.
    #[inline]
    pub fn dec(&self) {
        self.value.add(-1.0);
    }

    /// Adds `delta` (which may be negative).
    #[inline]
    pub fn add(&self, delta: f64) {
        self.value.add(delta);
    }

    /// Subtracts `delta`.
    #[inline]
    pub fn sub(&self, delta: f64) {
        self.value.add(-delta);
    }

    /// The current value.
    #[must_use]
    pub fn get(&self) -> f64 {
        self.value.load()
    }

    /// Increments now and decrements when the returned guard drops, so the
    /// gauge counts work in progress even when that work unwinds.
    ///
    /// ```
    /// use belay_metrics::Gauge;
    ///
    /// let running = Gauge::new();
    /// {
    ///     let _guard = running.track_in_flight();
    ///     assert_eq!(running.get(), 1.0);
    /// }
    /// assert_eq!(running.get(), 0.0);
    /// ```
    pub fn track_in_flight(&self) -> InFlight {
        self.inc();
        InFlight {
            gauge: self.clone(),
        }
    }
}

impl fmt::Debug for Gauge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Gauge").field("value", &self.get()).finish()
    }
}

/// Guard returned by [`Gauge::track_in_flight`]; decrements on drop.
#[must_use = "the gauge is decremented as soon as the guard is dropped"]
#[derive(Debug)]
pub struct InFlight {
    gauge: Gauge,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.gauge.dec();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic_is_applied_in_order() {
        let g = Gauge::new();
        g.set(2.0);
        g.add(0.5);
        g.dec();
        g.sub(-4.0);
        assert_eq!(g.get(), 5.5);
    }

    #[test]
    fn accepts_non_finite_values() {
        let g = Gauge::new();
        g.set(f64::NEG_INFINITY);
        assert_eq!(g.get(), f64::NEG_INFINITY);
        g.set(f64::NAN);
        assert!(g.get().is_nan());
    }

    #[test]
    fn in_flight_guard_decrements_when_work_panics() {
        let g = Gauge::new();
        let inner = g.clone();
        let result = std::panic::catch_unwind(move || {
            let _guard = inner.track_in_flight();
            panic!("handler failed");
        });
        assert!(result.is_err());
        assert_eq!(g.get(), 0.0);
    }
}
