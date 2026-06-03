//! Compact renderings of latencies, rates and sizes for the results
//! tables: three significant digits and an ASCII unit, so columns stay
//! narrow and line up on any terminal (`us`, not the micro sign).

use std::time::Duration;

/// Formats a value with three significant digits: `1.23`, `12.3`, `123`.
/// Values of 1000 and more keep all their integer digits.
fn three_digits(value: f64) -> String {
    if value < 10.0 {
        format!("{value:.2}")
    } else if value < 100.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.0}")
    }
}

/// Scales `value` by the largest step of 1000 that keeps it below 1000,
/// and appends that step's suffix.
fn scaled(value: f64, suffixes: &[&str]) -> String {
    let mut value = value;
    for (i, suffix) in suffixes.iter().enumerate() {
        if value < 1000.0 || i + 1 == suffixes.len() {
            return format!("{}{suffix}", three_digits(value));
        }
        value /= 1000.0;
    }
    unreachable!("the loop returns on its last suffix")
}

/// A latency in nanoseconds: `850ns`, `12.3us`, `4.56ms`, `1.23s`.
///
/// ```
/// use belay_bench::units::format_nanos;
///
/// assert_eq!(format_nanos(850), "850ns");
/// assert_eq!(format_nanos(12_345), "12.3us");
/// assert_eq!(format_nanos(4_560_000), "4.56ms");
/// ```
#[must_use]
pub fn format_nanos(ns: u64) -> String {
    if ns < 1_000 {
        return format!("{ns}ns");
    }
    scaled(ns as f64 / 1_000.0, &["us", "ms", "s"])
}

/// [`format_nanos`] for a [`Duration`].
#[must_use]
pub fn format_elapsed(d: Duration) -> String {
    format_nanos(u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// A count with an SI suffix: `950`, `12.3k`, `4.56M`.
#[must_use]
pub fn format_si(value: f64) -> String {
    if !value.is_finite() || value < 0.0 {
        return "-".to_owned();
    }
    scaled(value, &["", "k", "M", "G", "T"])
}

/// A count: exact below 1000 (`3`, `256`), then [`format_si`] (`16.4k`).
#[must_use]
pub fn format_count(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else {
        format_si(n as f64)
    }
}

/// A rate per second: `0.50/s`, `456k/s`, `1.23M/s`.
#[must_use]
pub fn format_rate(per_second: f64) -> String {
    format!("{}/s", format_si(per_second))
}
