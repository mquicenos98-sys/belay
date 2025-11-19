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

impl Observe for Summary {
    fn observe(&self, value: f64) {
        Summary::observe(self, value);
    }

    fn time_source(&self) -> &dyn TimeSource {
        &*self.core.time
    }
}

impl fmt::Debug for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Summary")
            .field("quantiles", &self.core.quantiles)
            .field("count", &self.count())
            .field("sum", &self.sum())
            .finish()
    }
}

/// The ring of sub-windows.
struct Window {
    /// Sub-window length in nanoseconds (at least 1ms).
    width: u64,
    capacity: usize,
    slots: Box<[Slot]>,
}

struct Slot {
    /// High 32 bits: low bits of the epoch this sub-window holds. Low 32
    /// bits: observations seen during it (saturating). Zero means unused.
    state: AtomicU64,
    /// Allocated on first use so idle children of a vec cost little.
    samples: OnceLock<Box<[AtomicU64]>>,
}

fn pack(tag: u32, seen: u32) -> u64 {
    (u64::from(tag) << 32) | u64::from(seen)
}

fn unpack(state: u64) -> (u32, u32) {
    ((state >> 32) as u32, state as u32)
}

/// Whether epoch tag `a` is later than `b`, tolerating wrap-around. Tags are
/// only ever compared across a few sub-windows, far below 2^31 apart.
fn is_later(a: u32, b: u32) -> bool {
    a.wrapping_sub(b) > 0
}

/// SplitMix64's output function: a cheap, well-mixed 64-bit hash.
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl Window {
    fn new(opts: &SummaryOpts) -> Window {
        let width = opts.max_age.as_nanos() / u128::from(opts.age_buckets);
        Window {
            width: u64::try_from(width).unwrap_or(u64::MAX).max(1),
            capacity: opts.reservoir_size,
            slots: (0..opts.age_buckets)
                .map(|_| Slot {
                    state: AtomicU64::new(0),
                    samples: OnceLock::new(),
                })
                .collect(),
        }
    }

    fn epoch(&self, now: Duration) -> u64 {
        u64::try_from(now.as_nanos() / u128::from(self.width)).unwrap_or(u64::MAX)
    }

    fn slot(&self, epoch: u64) -> &Slot {
        &self.slots[(epoch % self.slots.len() as u64) as usize]
    }

    fn record(&self, value: f64, now: Duration) {
        let epoch = self.epoch(now);
        let tag = epoch as u32;
        let slot = self.slot(epoch);

        let mut current = slot.state.load(Ordering::Acquire);
        let index = loop {
            let (held, seen) = unpack(current);
            let (next, index) = if held == tag && seen > 0 {
                (pack(tag, seen.saturating_add(1)), seen)
            } else if seen == 0 || is_later(tag, held) {
                // Unused, or holding an expired sub-window: take it over.
                (pack(tag, 1), 0)
            } else {
                // The slot already moved on to a later sub-window (the clock
                // went backwards or this thread stalled); the observation's
                // own sub-window is gone.
                return;
            };
            match slot.state.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break u64::from(index),
                Err(actual) => current = actual,
            }
        };

        let position = if index < self.capacity as u64 {
            index
        } else {
            // Algorithm R: the (index+1)-th observation replaces a random
            // retained sample with probability capacity / (index + 1).
            let draw = mix(epoch.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ index) % (index + 1);
            if draw >= self.capacity as u64 {
                return;
            }
            draw
        };
        let samples = slot
            .samples
            .get_or_init(|| (0..self.capacity).map(|_| AtomicU64::new(0)).collect());
        samples[position as usize].store(value.to_bits(), Ordering::Release);
    }

    /// Appends `(value, weight)` for every retained sample in the live
    /// sub-windows and returns the total weight.
    fn collect(&self, now: Duration, out: &mut Vec<(f64, f64)>) -> f64 {
        let current = self.epoch(now);
        let live = (self.slots.len() as u64).min(current.saturating_add(1));
        let mut total = 0.0;
        for age in 0..live {
            let epoch = current - age;
            let slot = self.slot(epoch);
            let (tag, seen) = unpack(slot.state.load(Ordering::Acquire));
            if tag != epoch as u32 || seen == 0 {
                continue;
            }
            let Some(samples) = slot.samples.get() else {
                continue;
            };
            let kept = (seen as usize).min(self.capacity);
            let weight = f64::from(seen) / kept as f64;
            out.extend(
                samples[..kept]
                    .iter()
                    .map(|s| (f64::from_bits(s.load(Ordering::Acquire)), weight)),
            );
            total += weight * kept as f64;
        }
        total
    }
}

/// Nearest-rank quantiles over weighted samples: the estimate for `q` is the
/// smallest sample whose cumulative weight reaches `q * total`. `quantiles`
/// must be sorted ascending; the result follows the same order.
fn weighted_quantiles(samples: &mut [(f64, f64)], total: f64, quantiles: &[f64]) -> Vec<Quantile> {
    samples.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out = Vec::with_capacity(quantiles.len());
    let mut i = 0;
    let mut below = 0.0;
    for &quantile in quantiles {
        let value = if samples.is_empty() {
            f64::NAN
        } else {
            // Shave a relative epsilon off the target: `q * total` can round
            // up past an exact rank (0.07 * 100 == 7.000000000000001), which
            // would skip to the next sample.
            let target = quantile * total * (1.0 - 1e-12);
            while i + 1 < samples.len() && below + samples[i].1 < target {
                below += samples[i].1;
                i += 1;
            }
            samples[i].0
        };
        out.push(Quantile { quantile, value });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ManualTime;

    fn summary(opts: SummaryOpts) -> (ManualTime, Summary) {
        let time = ManualTime::new();
        let s = Summary::with_time_source(opts, Arc::new(time.clone())).unwrap();
        (time, s)
    }

    fn values(s: &Summary) -> Vec<f64> {
        s.quantiles().iter().map(|q| q.value).collect()
    }

    #[test]
    fn quantiles_are_exact_within_reservoir_capacity() {
        let qs = [0.0, 0.07, 0.5, 0.9, 0.99, 1.0];
        let (_, s) = summary(SummaryOpts::new().quantiles(&qs));
        // Observe out of order: the estimate must not depend on arrival order.
        for v in (1..=100).rev() {
            s.observe(f64::from(v));
        }
        // 0.07 * 100 rounds up to 7.000000000000001 and must still give rank 7.
        assert_eq!(values(&s), [1.0, 7.0, 50.0, 90.0, 99.0, 100.0]);
        assert_eq!(s.count(), 100);
        assert_eq!(s.sum(), 5050.0);
    }

    #[test]
    fn empty_window_reports_nan() {
        let (_, s) = summary(SummaryOpts::new());
        assert!(values(&s).iter().all(|v| v.is_nan()));
        assert_eq!(s.snapshot().quantiles.len(), 3);
    }

    #[test]
    fn observations_expire_one_sub_window_at_a_time() {
        let opts = SummaryOpts::new()
            .quantiles(&[0.0])
            .max_age(Duration::from_secs(10))
            .age_buckets(5);
        let (time, s) = summary(opts);
        s.observe(1.0); // t=0, sub-window [0s, 2s)
        time.set(Duration::from_secs(3));
        s.observe(2.0); // sub-window [2s, 4s)

        time.set(Duration::from_millis(9_999));
        assert_eq!(values(&s), [1.0]);
        time.set(Duration::from_secs(10));
        assert_eq!(values(&s), [2.0]);
        time.set(Duration::from_secs(12));
        assert!(values(&s)[0].is_nan());
        // Lifetime totals are unaffected by expiry.
        assert_eq!((s.count(), s.sum()), (2, 3.0));
    }

    #[test]
    fn reused_slot_forgets_its_previous_sub_window() {
        let opts = SummaryOpts::new()
            .quantiles(&[0.0, 1.0])
            .max_age(Duration::from_secs(10))
            .age_buckets(5);
        let (time, s) = summary(opts);
        s.observe(5.0);
        s.observe(6.0);
        time.set(Duration::from_secs(10)); // epoch 5 maps onto epoch 0's slot
        s.observe(7.0);
        assert_eq!(values(&s), [7.0, 7.0]);
    }

    #[test]
    fn sampling_beyond_capacity_stays_close_and_is_deterministic() {
        let opts = SummaryOpts::new()
            .quantiles(&[0.1, 0.5, 0.9])
            .reservoir_size(128);
        let (_, a) = summary(opts.clone());
        let (_, b) = summary(opts);
        for i in 0..20_000u32 {
            // A scrambled but reproducible order of 0..20000.
            let v = f64::from((i * 7919) % 20_000);
            a.observe(v);
            b.observe(v);
        }
        let got = values(&a);
        assert_eq!(got, values(&b));
        for (estimate, truth) in got.iter().zip([2_000.0, 10_000.0, 18_000.0]) {
            assert!(
                (estimate - truth).abs() < 2_000.0,
                "estimate {estimate} too far from {truth}"
            );
        }
    }

    #[test]
    fn nan_is_counted_but_not_ranked() {
        let (_, s) = summary(SummaryOpts::new().quantiles(&[1.0]));
        s.observe(1.0);
        s.observe(f64::NAN);
        assert_eq!(values(&s), [1.0]);
        assert_eq!(s.count(), 2);
        assert!(s.sum().is_nan());
    }

    #[test]
    fn without_quantiles_only_totals_are_kept() {
        let (_, s) = summary(SummaryOpts::new().quantiles(&[]));
        s.observe(2.5);
        let snap = s.snapshot();
        assert!(snap.quantiles.is_empty());
        assert_eq!((snap.count, snap.sum), (1, 2.5));
    }

    #[test]
    fn options_are_validated_and_normalised() {
        let opts = SummaryOpts::new()
            .quantiles(&[0.99, 0.5, 0.5])
            .validated()
            .unwrap();
        assert_eq!(opts.get_quantiles(), [0.5, 0.99]);

        let bad = [
            SummaryOpts::new().quantiles(&[1.5]),
            SummaryOpts::new().quantiles(&[f64::NAN]),
            SummaryOpts::new().age_buckets(0),
            SummaryOpts::new().age_buckets(257),
            SummaryOpts::new().reservoir_size(0),
            SummaryOpts::new().max_age(Duration::from_micros(4_999)),
        ];
        for opts in bad {
            assert!(
                matches!(opts.clone().validated(), Err(Error::InvalidSummary(_))),
                "{opts:?} should be rejected"
            );
        }
        let err = SummaryOpts::new()
            .quantiles(&[-0.5])
            .validated()
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid summary options: quantiles must be within [0, 1], got -0.5"
        );
    }

    #[test]
    fn weighted_quantiles_respect_weights() {
        // 1.0 stands for 3 observations, 2.0 for 1.
        let mut samples = vec![(2.0, 1.0), (1.0, 3.0)];
        let q = weighted_quantiles(&mut samples, 4.0, &[0.5, 0.75, 0.76, 1.0]);
        let v: Vec<f64> = q.iter().map(|q| q.value).collect();
        assert_eq!(v, [1.0, 1.0, 2.0, 2.0]);
    }

    #[test]
    fn epoch_tags_compare_across_wrap_around() {
        assert!(is_later(1, 0));
        assert!(is_later(0, u32::MAX));
        assert!(!is_later(7, 7));
        assert_eq!(unpack(pack(0xdead_beef, 42)), (0xdead_beef, 42));
    }

    #[test]
    fn timer_records_into_the_summary() {
        let (time, s) = summary(SummaryOpts::new().quantiles(&[0.5]));
        let timer = s.start_timer();
        time.advance(Duration::from_millis(1500));
        drop(timer);
        assert_eq!(values(&s), [1.5]);
    }
}
