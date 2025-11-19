//! Summaries: quantile estimates over a sliding time window.
//!
//! The window (`max_age`) is split into `age_buckets` sub-windows arranged as
//! a ring. Each sub-window keeps a fixed-size reservoir sample (Vitter's
//! Algorithm R) of the observations made during it, together with how many
//! observations it saw. A scrape merges the reservoirs of the live
//! sub-windows, weighting every retained sample by `seen / retained`, and
//! reads quantiles off the weighted, sorted samples. While a sub-window has
//! seen no more observations than its reservoir holds, the estimate is the
//! exact nearest-rank quantile.
//!
//! The hot path takes no lock. A sub-window's epoch tag and observation count
//! share one `AtomicU64`, so claiming a reservoir index and rotating a stale
//! sub-window are a single compare-and-swap; the sample itself is then
//! stored into an atomic slot. The replacement index for Algorithm R comes
//! from hashing the epoch and observation index rather than from a shared
//! RNG, which keeps the hot path contention-free and makes the estimate a
//! pure function of the observation sequence and the clock, so tests with
//! [`ManualTime`](crate::ManualTime) are fully deterministic.
//!
//! The price of lock-freedom is a benign race: a scrape can read a sample
//! slot that a concurrent observer has claimed but not yet written, and a
//! writer delayed across a rotation can leave one stale value in the new
//! sub-window. Either way at most one sample per in-flight observation is
//! off, which is well inside the estimator's sampling error.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::atomic::AtomicF64;
use crate::error::{Error, Result};
use crate::snapshot::{Quantile, SummaryValue};
use crate::time::{self, SharedTimeSource, TimeSource};
use crate::timer::{Observe, SummaryTimer, Timer};

const MAX_AGE_BUCKETS: u32 = 256;

const MAX_RESERVOIR_SIZE: usize = 1 << 16;

const MIN_SUB_WINDOW: Duration = Duration::from_millis(1);

/// Configuration for a [`Summary`].
///
/// Observations are forgotten between `max_age - max_age / age_buckets` and
/// `max_age` after they were made: the oldest sub-window is dropped whole.
///
/// ```
/// use belay_metrics::SummaryOpts;
/// use std::time::Duration;
///
/// let opts = SummaryOpts::new()
///     .quantiles(&[0.5, 0.99])
///     .max_age(Duration::from_secs(60))
///     .age_buckets(6);
/// assert_eq!(opts.get_quantiles(), [0.5, 0.99]);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct SummaryOpts {
    quantiles: Vec<f64>,
    max_age: Duration,
    age_buckets: u32,
    reservoir_size: usize,
}

impl SummaryOpts {
    /// Quantiles 0.5, 0.9 and 0.99 over a 10 minute window of 5 sub-windows,
    /// with 512 retained samples per sub-window.
    pub fn new() -> Self {
        SummaryOpts {
            quantiles: vec![0.5, 0.9, 0.99],
            max_age: Duration::from_secs(600),
            age_buckets: 5,
            reservoir_size: 512,
        }
    }

    /// Quantiles to report, each in `[0, 1]`. Order and duplicates do not
    /// matter; an empty list reports only `_sum` and `_count`.
    #[must_use]
    pub fn quantiles(mut self, quantiles: &[f64]) -> Self {
        self.quantiles = quantiles.to_vec();
        self
    }

    /// Length of the sliding window.
    #[must_use]
    pub fn max_age(mut self, max_age: Duration) -> Self {
        self.max_age = max_age;
        self
    }

    /// Number of sub-windows the window is split into (1 to 256). More
    /// sub-windows make expiry smoother at the cost of memory.
    #[must_use]
    pub fn age_buckets(mut self, age_buckets: u32) -> Self {
        self.age_buckets = age_buckets;
        self
    }

    /// Samples retained per sub-window (1 to 65536). Estimates are exact
    /// until a sub-window sees more observations than this.
    #[must_use]
    pub fn reservoir_size(mut self, reservoir_size: usize) -> Self {
        self.reservoir_size = reservoir_size;
        self
    }

    /// The configured quantiles.
    pub fn get_quantiles(&self) -> &[f64] {
        &self.quantiles
    }

    /// The configured window length.
    pub fn get_max_age(&self) -> Duration {
        self.max_age
    }

    /// The configured number of sub-windows.
    pub fn get_age_buckets(&self) -> u32 {
        self.age_buckets
    }

    /// The configured reservoir size.
    pub fn get_reservoir_size(&self) -> usize {
        self.reservoir_size
    }

    /// Checks the options and normalises the quantile list (sorted,
    /// deduplicated) so equal configurations compare equal.
    pub(crate) fn validated(mut self) -> Result<SummaryOpts> {
        if let Some(&q) = self.quantiles.iter().find(|q| !(0.0..=1.0).contains(*q)) {
            return Err(Error::InvalidSummary(format!(
                "quantiles must be within [0, 1], got {q}"
            )));
        }
        self.quantiles.sort_by(f64::total_cmp);
        self.quantiles.dedup();
        if !(1..=MAX_AGE_BUCKETS).contains(&self.age_buckets) {
            return Err(Error::InvalidSummary(format!(
                "age buckets must be 1-{MAX_AGE_BUCKETS}, got {}",
                self.age_buckets
            )));
        }
        if !(1..=MAX_RESERVOIR_SIZE).contains(&self.reservoir_size) {
            return Err(Error::InvalidSummary(format!(
                "reservoir size must be 1-{MAX_RESERVOIR_SIZE}, got {}",
                self.reservoir_size
            )));
        }
        if self.max_age / self.age_buckets < MIN_SUB_WINDOW {
            return Err(Error::InvalidSummary(format!(
                "max age {:?} split into {} buckets gives sub-windows under 1ms",
                self.max_age, self.age_buckets
            )));
        }
        Ok(self)
    }
}

impl Default for SummaryOpts {
    fn default() -> Self {
        Self::new()
    }
}

/// Tracks the distribution of observations as quantiles over a sliding
/// window, plus a lifetime sum and count.
///
/// Prefer a [`Histogram`](crate::Histogram) when series from several
/// processes need aggregating: summary quantiles cannot be averaged. A
/// summary fits single-process latency reporting where the buckets of a
/// histogram are not known in advance.
///
/// `NaN` observations count towards `_count` and turn `_sum` into `NaN` but
/// are left out of the quantile window.
#[derive(Clone)]
pub struct Summary {
    core: Arc<Core>,
}

struct Core {
    quantiles: Box<[f64]>,
    window: Window,
    count: AtomicU64,
    sum: AtomicF64,
    time: SharedTimeSource,
}

impl Summary {
    /// A standalone summary using the process-wide monotonic clock.
    pub fn new(opts: SummaryOpts) -> Result<Summary> {
        Self::with_time_source(opts, time::monotonic())
    }

    /// A standalone summary whose window and timers follow `time`.
    pub fn with_time_source(opts: SummaryOpts, time: SharedTimeSource) -> Result<Summary> {
        Ok(Self::from_validated(&opts.validated()?, time))
    }

    /// Builds from options that already passed [`SummaryOpts::validated`].
    pub(crate) fn from_validated(opts: &SummaryOpts, time: SharedTimeSource) -> Summary {
        Summary {
            core: Arc::new(Core {
                quantiles: opts.quantiles.clone().into_boxed_slice(),
                window: Window::new(opts),
                count: AtomicU64::new(0),
                sum: AtomicF64::zero(),
                time,
            }),
        }
    }

    /// Records one observation.
    pub fn observe(&self, value: f64) {
        let core = &*self.core;
        core.count.fetch_add(1, Ordering::Relaxed);
        core.sum.add(value);
        if !value.is_nan() && !core.quantiles.is_empty() {
            core.window.record(value, core.time.now());
        }
    }

    /// Records a duration in seconds.
    pub fn observe_duration(&self, elapsed: Duration) {
        self.observe(elapsed.as_secs_f64());
    }

    /// Starts a timer that records into this summary when stopped or dropped.
    pub fn start_timer(&self) -> SummaryTimer {
        Timer::start(self.clone())
    }

    /// Number of observations ever made.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.core.count.load(Ordering::Relaxed)
    }

    /// Sum of all observations ever made.
    #[must_use]
    pub fn sum(&self) -> f64 {
        self.core.sum.load()
    }

    /// Current estimates for the configured quantiles, in increasing order.
    /// Values are `NaN` while the window is empty.
    pub fn quantiles(&self) -> Vec<Quantile> {
        let core = &*self.core;
        if core.quantiles.is_empty() {
            return Vec::new();
        }
        let mut samples = Vec::new();
        let total = core.window.collect(core.time.now(), &mut samples);
        weighted_quantiles(&mut samples, total, &core.quantiles)
    }

    /// Quantiles, sum and count as of now.
    pub fn snapshot(&self) -> SummaryValue {
        // Read the lifetime totals first: a racing observation then shows up
        // in the window no earlier than in the count.
        let count = self.count();
        let sum = self.sum();
        SummaryValue {
            quantiles: self.quantiles(),
            sum,
            count,
        }
    }
}
