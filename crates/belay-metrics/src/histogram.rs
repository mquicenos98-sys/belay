//! Histograms with fixed bucket bounds, and helpers to generate bounds.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::atomic::AtomicF64;
use crate::error::{Error, Result};
use crate::snapshot::{Bucket, HistogramValue};
use crate::time::{self, SharedTimeSource, TimeSource};
use crate::timer::{HistogramTimer, Observe, Timer};

/// The Prometheus client default bounds, in seconds: 5ms to 10s. A decent
/// first guess for request latencies; pick real bounds once you know the
/// distribution.
pub const DEFAULT_BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// `count` bounds starting at `start`, each `factor` times the previous.
///
/// ```
/// use belay_metrics::exponential_buckets;
///
/// assert_eq!(exponential_buckets(0.5, 2.0, 4).unwrap(), [0.5, 1.0, 2.0, 4.0]);
/// assert!(exponential_buckets(0.0, 2.0, 4).is_err());
/// ```
pub fn exponential_buckets(start: f64, factor: f64, count: usize) -> Result<Vec<f64>> {
    if count == 0 {
        return Err(Error::InvalidBuckets("count must be at least 1".into()));
    }
    if !(start.is_finite() && start > 0.0) {
        return Err(Error::InvalidBuckets(format!(
            "exponential start must be a positive finite number, got {start}"
        )));
    }
    if !(factor.is_finite() && factor > 1.0) {
        return Err(Error::InvalidBuckets(format!(
            "exponential factor must be greater than 1, got {factor}"
        )));
    }
    let mut out = Vec::with_capacity(count);
    let mut bound = start;
    for _ in 0..count {
        if !bound.is_finite() {
            return Err(Error::InvalidBuckets(format!(
                "{count} exponential buckets from {start} by {factor} overflow f64"
            )));
        }
        out.push(bound);
        bound *= factor;
    }
    Ok(out)
}

/// `count` bounds starting at `start`, `width` apart.
///
/// ```
/// use belay_metrics::linear_buckets;
///
/// assert_eq!(linear_buckets(10.0, 5.0, 3).unwrap(), [10.0, 15.0, 20.0]);
/// ```
pub fn linear_buckets(start: f64, width: f64, count: usize) -> Result<Vec<f64>> {
    if count == 0 {
        return Err(Error::InvalidBuckets("count must be at least 1".into()));
    }
    if !start.is_finite() {
        return Err(Error::InvalidBuckets(format!(
            "linear start must be finite, got {start}"
        )));
    }
    if !(width.is_finite() && width > 0.0) {
        return Err(Error::InvalidBuckets(format!(
            "linear width must be a positive finite number, got {width}"
        )));
    }
    // Multiplying instead of accumulating keeps rounding error from growing
    // with the bucket index.
    let out: Vec<f64> = (0..count).map(|i| start + width * i as f64).collect();
    match out.last() {
        Some(last) if last.is_finite() => Ok(out),
        _ => Err(Error::InvalidBuckets(format!(
            "{count} linear buckets from {start} by {width} overflow f64"
        ))),
    }
}

/// Checks user-supplied bounds: finite and strictly increasing. A trailing
/// `+Inf` is accepted and dropped because the `+Inf` bucket always exists.
pub(crate) fn validate_buckets(bounds: &[f64]) -> Result<Arc<[f64]>> {
    let bounds = match bounds.split_last() {
        Some((&last, rest)) if last == f64::INFINITY => rest,
        _ => bounds,
    };
    for (i, &b) in bounds.iter().enumerate() {
        if !b.is_finite() {
            return Err(Error::InvalidBuckets(format!(
                "bound {b} is not finite (only a trailing +Inf is allowed)"
            )));
        }
        if i > 0 && b <= bounds[i - 1] {
            return Err(Error::InvalidBuckets(format!(
                "bounds must be strictly increasing, got {b} after {}",
                bounds[i - 1]
            )));
        }
    }
    Ok(bounds.into())
}

/// Counts observations into buckets, e.g. request latencies in seconds.
///
/// Each observation increments exactly one per-bucket atomic counter and adds
/// to an atomic sum; the cumulative `le` counts Prometheus expects are
/// computed at scrape time. The total count is the sum of the buckets, so the
/// exported `_count` always equals the `+Inf` bucket even while observations
/// race with a scrape (the `_sum` may be one observation ahead or behind).
///
/// `NaN` lands in the `+Inf` bucket and turns the sum into `NaN`, matching
/// the reference Go client.
///
/// ```
/// use belay_metrics::Histogram;
///
/// let h = Histogram::new(&[0.1, 1.0]).unwrap();
/// h.observe(0.05);
/// h.observe(3.0);
/// let snap = h.snapshot();
/// let cumulative: Vec<u64> = snap.buckets.iter().map(|b| b.cumulative_count).collect();
/// assert_eq!(cumulative, [1, 1, 2]);
/// assert_eq!(snap.count, 2);
/// ```
#[derive(Clone)]
pub struct Histogram {
    core: Arc<Core>,
}

struct Core {
    bounds: Arc<[f64]>,
    /// Non-cumulative counts; index `bounds.len()` is the `+Inf` bucket.
    counts: Box<[AtomicU64]>,
    sum: AtomicF64,
    time: SharedTimeSource,
}

impl Histogram {
    /// A standalone histogram with the given bucket upper bounds, timed by
    /// the process-wide monotonic clock.
    pub fn new(buckets: &[f64]) -> Result<Histogram> {
        Self::with_time_source(buckets, time::monotonic())
    }

    /// Like [`new`](Self::new) but timers read `time`.
    pub fn with_time_source(buckets: &[f64], time: SharedTimeSource) -> Result<Histogram> {
        Ok(Self::from_bounds(validate_buckets(buckets)?, time))
    }

    /// Builds from bounds that already passed [`validate_buckets`].
    pub(crate) fn from_bounds(bounds: Arc<[f64]>, time: SharedTimeSource) -> Histogram {
        let counts = (0..=bounds.len()).map(|_| AtomicU64::new(0)).collect();
        Histogram {
            core: Arc::new(Core {
                bounds,
                counts,
                sum: AtomicF64::zero(),
                time,
            }),
        }
    }

    /// Records one observation.
    #[inline]
    pub fn observe(&self, value: f64) {
        let core = &*self.core;
        // `!(value < b)` rather than `b <= value` so NaN, which compares
        // false with everything, falls through to the +Inf bucket.
        let index = core.bounds.partition_point(|&b| !(value < b));
        core.counts[index].fetch_add(1, Ordering::Relaxed);
        core.sum.add(value);
    }

    /// Records a duration in seconds.
    #[inline]
    pub fn observe_duration(&self, elapsed: Duration) {
        self.observe(elapsed.as_secs_f64());
    }

    /// Starts a timer that records into this histogram when stopped or dropped.
    pub fn start_timer(&self) -> HistogramTimer {
        Timer::start(self.clone())
    }

    /// Runs `f` and records how long it took.
    pub fn time<R>(&self, f: impl FnOnce() -> R) -> R {
        let timer = self.start_timer();
        let out = f();
        timer.stop_and_record();
        out
    }

    /// The finite bucket upper bounds (the `+Inf` bucket is implicit).
    pub fn buckets(&self) -> &[f64] {
        &self.core.bounds
    }

    /// Number of observations so far.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.core
            .counts
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .sum()
    }

    /// Sum of observations so far.
    #[must_use]
    pub fn sum(&self) -> f64 {
        self.core.sum.load()
    }

    /// Cumulative buckets, sum and count as of now.
    pub fn snapshot(&self) -> HistogramValue {
        let core = &*self.core;
        let mut cumulative = 0u64;
        let buckets = core
            .counts
            .iter()
            .enumerate()
            .map(|(i, c)| {
                cumulative += c.load(Ordering::Relaxed);
                Bucket {
                    upper_bound: core.bounds.get(i).copied().unwrap_or(f64::INFINITY),
                    cumulative_count: cumulative,
                }
            })
            .collect();
        HistogramValue {
            buckets,
            sum: core.sum.load(),
            count: cumulative,
        }
    }
}

impl Observe for Histogram {
    fn observe(&self, value: f64) {
        Histogram::observe(self, value);
    }

    fn time_source(&self) -> &dyn TimeSource {
        &*self.core.time
    }
}

impl fmt::Debug for Histogram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Histogram")
            .field("buckets", &self.buckets())
            .field("count", &self.count())
            .field("sum", &self.sum())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cumulative(h: &Histogram) -> Vec<u64> {
        h.snapshot()
            .buckets
            .iter()
            .map(|b| b.cumulative_count)
            .collect()
    }

    #[test]
    fn nan_goes_to_the_inf_bucket_and_poisons_the_sum() {
        let h = Histogram::new(&[1.0]).unwrap();
        h.observe(0.5);
        h.observe(f64::NAN);
        assert_eq!(cumulative(&h), [1, 2]);
        assert!(h.sum().is_nan());
    }

    #[test]
    fn infinities_land_in_the_outer_buckets() {
        let h = Histogram::new(&[0.0]).unwrap();
        h.observe(f64::NEG_INFINITY);
        h.observe(f64::INFINITY);
        assert_eq!(cumulative(&h), [1, 2]);
    }

    #[test]
    fn empty_bounds_leave_only_the_inf_bucket() {
        let h = Histogram::new(&[]).unwrap();
        h.observe(42.0);
        let snap = h.snapshot();
        assert_eq!(snap.buckets.len(), 1);
        assert_eq!(snap.buckets[0].upper_bound, f64::INFINITY);
        assert_eq!(snap.buckets[0].cumulative_count, 1);
    }

    #[test]
    fn trailing_inf_bound_is_dropped() {
        let h = Histogram::new(&[1.0, f64::INFINITY]).unwrap();
        assert_eq!(h.buckets(), [1.0]);
        assert_eq!(h.snapshot().buckets.len(), 2);
    }

    #[test]
    fn rejects_unsorted_duplicate_or_non_finite_bounds() {
        for bad in [
            &[1.0, 0.5][..],
            &[1.0, 1.0],
            &[f64::NAN],
            &[f64::NEG_INFINITY, 0.0],
            &[f64::INFINITY, f64::INFINITY],
        ] {
            assert!(
                matches!(Histogram::new(bad), Err(Error::InvalidBuckets(_))),
                "{bad:?} should be rejected"
            );
        }
        let err = Histogram::new(&[1.0, 0.5]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid histogram buckets: bounds must be strictly increasing, got 0.5 after 1"
        );
    }

    #[test]
    fn observe_duration_uses_seconds() {
        let h = Histogram::new(&DEFAULT_BUCKETS).unwrap();
        h.observe_duration(Duration::from_millis(30));
        assert_eq!(h.sum(), 0.03);
        assert_eq!(h.snapshot().buckets[3].cumulative_count, 1); // le=0.05
        assert_eq!(h.snapshot().buckets[2].cumulative_count, 0); // le=0.025
    }

    #[test]
    fn exponential_buckets_validate_their_arguments() {
        assert!(exponential_buckets(1.0, 1.0, 3).is_err());
        assert!(exponential_buckets(1.0, 2.0, 0).is_err());
        assert!(exponential_buckets(-1.0, 2.0, 3).is_err());
        assert!(exponential_buckets(1e300, 1e10, 3).is_err());
        assert_eq!(
            exponential_buckets(1.0, 10.0, 3).unwrap(),
            [1.0, 10.0, 100.0]
        );
    }

    #[test]
    fn linear_buckets_validate_their_arguments() {
        assert!(linear_buckets(0.0, 0.0, 3).is_err());
        assert!(linear_buckets(f64::NAN, 1.0, 3).is_err());
        assert!(linear_buckets(0.0, 1.0, 0).is_err());
        assert!(linear_buckets(f64::MAX, f64::MAX, 3).is_err());
        assert_eq!(linear_buckets(-1.0, 1.0, 3).unwrap(), [-1.0, 0.0, 1.0]);
    }

    #[test]
    fn bucket_helper_output_is_accepted_by_histograms() {
        let bounds = linear_buckets(0.1, 0.1, 50).unwrap();
        assert!(Histogram::new(&bounds).is_ok());
        let bounds = exponential_buckets(0.001, 1.5, 40).unwrap();
        assert!(Histogram::new(&bounds).is_ok());
    }
}
