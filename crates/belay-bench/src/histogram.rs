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
