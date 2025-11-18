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
