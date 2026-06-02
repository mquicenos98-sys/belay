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
