//! A latency histogram with log-linear buckets, in the style of
//! HdrHistogram.
//!
//! Values below `2^p` (`p` = precision bits) get a bucket each. Above
//! that, every power-of-two range `[2^m, 2^(m+1))` is split into `2^p`
//! equal sub-buckets, so a bucket is never wider than `2^-p` of the values
//! it holds. A percentile is reported as the upper edge of the bucket the
//! requested rank falls in (clamped to the exact minimum and maximum), so
//! it is never below the exact value and exceeds it by less than `2^-p`
//! of it: with the default 7 bits, less than 0.79%, whatever the
//! magnitude. Covering 1 ns to 584 years takes at most 7,424 counters,
//! allocated as the largest value grows; latencies under 10 ms use about
//! 2,300.

use std::fmt;
use std::time::Duration;

/// Precision used by [`Histogram::new`]: 128 sub-buckets per power of two.
pub const DEFAULT_PRECISION: u32 = 7;

/// Largest accepted precision. 16 bits already means 3.7 million counters
/// for the full `u64` range.
pub const MAX_PRECISION: u32 = 16;

/// Counts of `u64` values (nanoseconds, by convention) in log-linear
/// buckets, with exact count, minimum, maximum and mean.
///
/// ```
/// use belay_bench::histogram::Histogram;
///
/// let mut h = Histogram::new();
/// for us in 1..=1000u64 {
///     h.record(us * 1000); // 1us ..= 1ms, in nanoseconds
/// }
/// let p99 = h.percentile(99.0).unwrap();
/// assert!(p99 >= 990_000 && p99 < 990_000 + 990_000 / 128);
/// assert_eq!(h.max(), Some(1_000_000));
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct Histogram {
    precision: u32,
    counts: Vec<u64>,
    total: u64,
    min: u64,
    max: u64,
    sum: u128,
}

impl Histogram {
    /// An empty histogram with [`DEFAULT_PRECISION`].
    #[must_use]
    pub fn new() -> Histogram {
        Histogram::with_precision(DEFAULT_PRECISION)
    }

    /// An empty histogram with `2^bits` sub-buckets per power of two.
    ///
    /// # Panics
    ///
    /// Panics unless `1 <= bits <= MAX_PRECISION`; the precision is a
    /// constant of the calling code, not an input.
    #[must_use]
    pub fn with_precision(bits: u32) -> Histogram {
        assert!(
            (1..=MAX_PRECISION).contains(&bits),
            "histogram precision must be 1-{MAX_PRECISION} bits, got {bits}"
        );
        Histogram {
            precision: bits,
            counts: Vec::new(),
            total: 0,
            min: u64::MAX,
            max: 0,
            sum: 0,
        }
    }

    /// The precision in bits.
    #[must_use]
    pub fn precision(&self) -> u32 {
        self.precision
    }

    /// Upper bound on the relative error of a reported percentile:
    /// `2^-precision`.
    #[must_use]
    pub fn relative_error(&self) -> f64 {
        1.0 / (1u64 << self.precision) as f64
    }

    /// Records one occurrence of `value`.
    pub fn record(&mut self, value: u64) {
        self.record_n(value, 1);
    }

    /// Records a duration in nanoseconds, saturating at `u64::MAX`.
    pub fn record_duration(&mut self, d: Duration) {
        self.record(u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
    }

    /// Records `n` occurrences of `value`.
    pub fn record_n(&mut self, value: u64, n: u64) {
        if n == 0 {
            return;
        }
        let index = self.index_of(value);
        if index >= self.counts.len() {
            self.counts.resize(index + 1, 0);
        }
        self.counts[index] += n;
        self.total += n;
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        self.sum += u128::from(value) * u128::from(n);
    }

    /// Adds every value recorded in `other`.
    ///
    /// # Panics
    ///
    /// Panics if the precisions differ: the buckets would not line up.
    pub fn merge(&mut self, other: &Histogram) {
        assert_eq!(
            self.precision, other.precision,
            "cannot merge histograms of different precision"
        );
        if other.counts.len() > self.counts.len() {
            self.counts.resize(other.counts.len(), 0);
        }
        for (mine, theirs) in self.counts.iter_mut().zip(&other.counts) {
            *mine += theirs;
        }
        self.total += other.total;
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
        self.sum += other.sum;
    }

    /// Number of recorded values.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.total
    }

    /// Whether nothing has been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// The exact smallest recorded value.
    #[must_use]
    pub fn min(&self) -> Option<u64> {
        (self.total > 0).then_some(self.min)
    }

    /// The exact largest recorded value.
    #[must_use]
    pub fn max(&self) -> Option<u64> {
        (self.total > 0).then_some(self.max)
    }

    /// The exact arithmetic mean.
    #[must_use]
    pub fn mean(&self) -> Option<f64> {
        (self.total > 0).then(|| self.sum as f64 / self.total as f64)
    }

    /// The value at quantile `q` (0.0 to 1.0, clamped): the smallest value
    /// at least `q` of the recorded values are less than or equal to,
    /// rounded up to its bucket's upper edge. Quantile 0 is the exact
    /// minimum and quantile 1 the exact maximum.
    #[must_use]
    pub fn quantile(&self, q: f64) -> Option<u64> {
        if self.total == 0 {
            return None;
        }
        let q = if q.is_nan() { 0.0 } else { q.clamp(0.0, 1.0) };
        let rank = ((q * self.total as f64).ceil() as u64).clamp(1, self.total);
        if rank == 1 {
            return Some(self.min);
        }
        if rank == self.total {
            return Some(self.max);
        }
        let mut seen = 0u64;
        for (index, &count) in self.counts.iter().enumerate() {
            seen += count;
            if seen >= rank {
                let (_, high) = self.bucket_bounds(index);
                return Some(high.clamp(self.min, self.max));
            }
        }
        Some(self.max)
    }

    /// [`quantile`](Self::quantile) with `p` in percent.
    #[must_use]
    pub fn percentile(&self, p: f64) -> Option<u64> {
        self.quantile(p / 100.0)
    }

    /// The usual latency summary, or `None` if nothing was recorded.
    #[must_use]
    pub fn summary(&self) -> Option<Summary> {
        Some(Summary {
            count: self.total,
            min: self.min()?,
            mean: self.mean()?,
            p50: self.percentile(50.0)?,
            p90: self.percentile(90.0)?,
            p99: self.percentile(99.0)?,
            p999: self.percentile(99.9)?,
            max: self.max()?,
        })
    }

    /// Bucket index of `value`. Monotonic in `value`.
    fn index_of(&self, value: u64) -> usize {
        let p = self.precision;
        let sub = 1u64 << p;
        if value < sub {
            return value as usize;
        }
        let msb = 63 - value.leading_zeros();
        let shift = msb - p;
        // Group `msb - p + 1` holds [2^msb, 2^(msb+1)); within it the
        // `p` bits after the leading one pick the sub-bucket.
        (((msb - p + 1) as usize) << p) | ((value >> shift) & (sub - 1)) as usize
    }

    /// Lowest and highest value (inclusive) that map to bucket `index`.
    fn bucket_bounds(&self, index: usize) -> (u64, u64) {
        let p = self.precision;
        let sub = 1usize << p;
        if index < sub {
            return (index as u64, index as u64);
        }
        let shift = (index >> p) as u32 - 1;
        let low = ((sub + (index & (sub - 1))) as u64) << shift;
        (low, low + ((1u64 << shift) - 1))
    }
}

impl Default for Histogram {
    fn default() -> Histogram {
        Histogram::new()
    }
}

impl fmt::Debug for Histogram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Histogram")
            .field("precision", &self.precision)
            .field("count", &self.total)
            .field("min", &self.min())
            .field("max", &self.max())
            .finish_non_exhaustive()
    }
}

/// Count, extremes, mean and the percentiles reported for every case.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Summary {
    /// Number of values.
    pub count: u64,
    /// Exact minimum.
    pub min: u64,
    /// Exact mean.
    pub mean: f64,
    /// Median.
    pub p50: u64,
    /// 90th percentile.
    pub p90: u64,
    /// 99th percentile.
    pub p99: u64,
    /// 99.9th percentile.
    pub p999: u64,
    /// Exact maximum.
    pub max: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_common::Rng;

    /// The exact value at quantile `q` of sorted `values`, with the same
    /// rank rule as `Histogram::quantile`.
    fn exact_quantile(sorted: &[u64], q: f64) -> u64 {
        let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
        sorted[rank - 1]
    }

    /// Log-uniform values between 1 and 2^max_bits, like latencies that
    /// span several orders of magnitude.
    fn log_uniform(rng: &mut Rng, n: usize, max_bits: u32) -> Vec<u64> {
        (0..n)
            .map(|_| {
                let bits = rng.gen_range(0..max_bits);
                (1u64 << bits) | (rng.next_u64() & ((1u64 << bits) - 1))
            })
            .collect()
    }

    #[test]
    fn small_values_are_recorded_exactly() {
        let mut h = Histogram::new();
        for v in 0..128 {
            h.record(v);
        }
        assert_eq!(h.quantile(0.5), Some(63));
        assert_eq!(h.percentile(90.0), Some(115));
        assert_eq!(h.min(), Some(0));
        assert_eq!(h.max(), Some(127));
        assert_eq!(h.mean(), Some(63.5));
    }

    #[test]
    fn every_value_lies_within_its_bucket_and_buckets_are_narrow() {
        let mut rng = Rng::seed_from_u64(1);
        for precision in [1, 3, 7, 12] {
            let h = Histogram::with_precision(precision);
            let mut values = log_uniform(&mut rng, 2_000, 64);
            values.extend([0, 1, u64::MAX, u64::MAX - 1, 1 << 63, (1 << precision) - 1]);
            for v in values {
                let (low, high) = h.bucket_bounds(h.index_of(v));
                assert!(low <= v && v <= high, "{v} outside [{low}, {high}]");
                let width = high - low;
                assert!(
                    width <= v >> precision,
                    "bucket of {v} is {width} wide at precision {precision}"
                );
            }
        }
    }

    #[test]
    fn bucket_index_is_monotonic() {
        let h = Histogram::with_precision(4);
        let mut last = 0;
        for v in (0..100_000u64).chain([u64::MAX / 2, u64::MAX]) {
            let index = h.index_of(v);
            assert!(index >= last, "index of {v} went backwards");
            last = index;
        }
        // The top bucket is the last one a full-range histogram needs.
        assert_eq!(h.index_of(u64::MAX), (65 - 4) * 16 - 1);
    }

    #[test]
    fn percentiles_stay_within_the_relative_error_bound() {
        let mut rng = Rng::seed_from_u64(0x5EED);
        for precision in [2, 5, DEFAULT_PRECISION] {
            let mut h = Histogram::with_precision(precision);
            let mut values = log_uniform(&mut rng, 50_000, 34);
            for &v in &values {
                h.record(v);
            }
            values.sort_unstable();
            for q in [0.0, 0.01, 0.25, 0.5, 0.75, 0.9, 0.99, 0.999, 0.9999, 1.0] {
                let exact = exact_quantile(&values, q);
                let reported = h.quantile(q).unwrap();
                let bound = exact as f64 * h.relative_error();
                assert!(
                    reported >= exact && (reported - exact) as f64 <= bound,
                    "q={q} precision={precision}: reported {reported}, exact {exact}"
                );
            }
        }
    }

    #[test]
    fn extremes_and_mean_are_exact() {
        let mut h = Histogram::new();
        for v in [1_234_567u64, 89, 1_000_000_007] {
            h.record(v);
        }
        assert_eq!(h.quantile(0.0), Some(89));
        assert_eq!(h.quantile(1.0), Some(1_000_000_007));
        assert_eq!(h.mean(), Some((1_234_567.0 + 89.0 + 1_000_000_007.0) / 3.0));
    }

    #[test]
    fn largest_values_do_not_overflow() {
        let mut h = Histogram::new();
        h.record(u64::MAX);
        h.record(u64::MAX - 5);
        h.record(0);
        assert_eq!(h.max(), Some(u64::MAX));
        assert_eq!(h.percentile(50.0), Some(u64::MAX));
        assert_eq!(h.mean(), Some((2.0 * u64::MAX as f64 - 5.0) / 3.0));
    }

    #[test]
    fn empty_histograms_report_nothing() {
        let h = Histogram::new();
        assert!(h.is_empty());
        assert_eq!(h.quantile(0.5), None);
        assert_eq!(h.mean(), None);
        assert_eq!(h.summary(), None);
    }

    #[test]
    fn merging_equals_recording_everything_in_one() {
        let mut rng = Rng::seed_from_u64(9);
        let values = log_uniform(&mut rng, 3_000, 40);
        let (mut a, mut b, mut all) = (Histogram::new(), Histogram::new(), Histogram::new());
        for (i, &v) in values.iter().enumerate() {
            let part = if i % 3 == 0 { &mut a } else { &mut b };
            part.record(v);
            all.record(v);
        }
        a.merge(&b);
        assert_eq!(a, all);
        // Merging into an empty histogram, whose counter vector is shorter.
        let mut empty = Histogram::new();
        empty.merge(&all);
        assert_eq!(empty, all);
    }

    #[test]
    fn weighted_records_count_every_occurrence() {
        let mut h = Histogram::new();
        h.record_n(10, 99);
        h.record_n(5_000, 1);
        h.record_n(7, 0);
        assert_eq!(h.count(), 100);
        assert_eq!(h.percentile(99.0), Some(10));
        assert_eq!(h.percentile(99.5), Some(5_000));
        assert_eq!(h.min(), Some(10), "a zero-weight record is not a value");
    }

    #[test]
    fn durations_are_recorded_in_nanoseconds() {
        let mut h = Histogram::new();
        h.record_duration(Duration::from_micros(3));
        h.record_duration(Duration::MAX);
        assert_eq!(h.min(), Some(3_000));
        assert_eq!(h.max(), Some(u64::MAX));
    }

    #[test]
    fn summary_orders_its_percentiles() {
        let mut rng = Rng::seed_from_u64(3);
        let mut h = Histogram::new();
        for v in log_uniform(&mut rng, 10_000, 30) {
            h.record(v);
        }
        let s = h.summary().unwrap();
        assert_eq!(s.count, 10_000);
        assert!(s.min <= s.p50 && s.p50 <= s.p90 && s.p90 <= s.p99);
        assert!(s.p99 <= s.p999 && s.p999 <= s.max);
    }

    #[test]
    #[should_panic(expected = "histogram precision must be 1-16 bits, got 0")]
    fn zero_precision_is_rejected() {
        let _ = Histogram::with_precision(0);
    }

    #[test]
    #[should_panic(expected = "different precision")]
    fn merging_different_precisions_panics() {
        Histogram::with_precision(3).merge(&Histogram::with_precision(4));
    }
}
