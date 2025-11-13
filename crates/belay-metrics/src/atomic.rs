//! An `f64` cell stored as raw bits in an `AtomicU64`.
//!
//! Every metric value is independent of every other, so all operations use
//! `Relaxed` ordering: a scrape observes each cell atomically but makes no
//! promise about the relative order of updates to different cells.

use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) struct AtomicF64(AtomicU64);

impl AtomicF64 {
    pub(crate) const fn zero() -> Self {
        // 0.0_f64 has an all-zero bit pattern.
        AtomicF64(AtomicU64::new(0))
    }

    #[inline]
    pub(crate) fn load(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }

    #[inline]
    pub(crate) fn store(&self, value: f64) {
        self.0.store(value.to_bits(), Ordering::Relaxed);
    }

    /// Adds `delta` with a compare-and-swap loop and returns the new value.
    #[inline]
    pub(crate) fn add(&self, delta: f64) -> f64 {
        let mut current = self.0.load(Ordering::Relaxed);
        loop {
            let next = f64::from_bits(current) + delta;
            match self.0.compare_exchange_weak(
                current,
                next.to_bits(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return next,
                Err(actual) => current = actual,
            }
        }
    }
}
