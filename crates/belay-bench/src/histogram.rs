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
