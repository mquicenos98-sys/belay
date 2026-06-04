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

/// A byte size in binary units without a space, so it can be a parameter
/// value: `100B`, `64KiB`, `1.5MiB`.
///
/// ```
/// use belay_bench::units::format_size;
///
/// assert_eq!(format_size(128), "128B");
/// assert_eq!(format_size(8 << 20), "8MiB");
/// assert_eq!(format_size(1536), "1.5KiB");
/// ```
#[must_use]
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    let mut unit = 1024u64;
    let mut index = 0;
    while index + 1 < UNITS.len() && bytes >= unit * 1024 {
        unit *= 1024;
        index += 1;
    }
    if bytes % unit == 0 {
        format!("{}{}", bytes / unit, UNITS[index])
    } else {
        format!("{:.1}{}", bytes as f64 / unit as f64, UNITS[index])
    }
}

/// Megabytes (10^6 bytes) per second with one decimal, as storage and
/// network benchmarks usually report them.
#[must_use]
pub fn format_mb_per_sec(bytes: u64, elapsed: Duration) -> String {
    let secs = elapsed.as_secs_f64();
    if bytes == 0 || secs <= 0.0 {
        return "-".to_owned();
    }
    format!("{:.1}", bytes as f64 / 1e6 / secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nanoseconds_use_three_significant_digits() {
        assert_eq!(format_nanos(0), "0ns");
        assert_eq!(format_nanos(999), "999ns");
        assert_eq!(format_nanos(1_000), "1.00us");
        assert_eq!(format_nanos(1_234), "1.23us");
        assert_eq!(format_nanos(99_940), "99.9us");
        assert_eq!(format_nanos(123_456), "123us");
        assert_eq!(format_nanos(1_500_000_000), "1.50s");
        assert_eq!(format_nanos(4_000_000_000_000), "4000s");
    }

    #[test]
    fn elapsed_durations_saturate() {
        assert_eq!(format_elapsed(Duration::from_millis(250)), "250ms");
        assert_eq!(format_elapsed(Duration::MAX), format_nanos(u64::MAX));
    }

    #[test]
    fn counts_are_exact_until_they_need_a_suffix() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(3), "3");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1.00k");
        assert_eq!(format_count(16_384), "16.4k");
        assert_eq!(format_count(u64::MAX), "18446744T");
    }

    #[test]
    fn rates_carry_si_suffixes() {
        assert_eq!(format_rate(0.5), "0.50/s");
        assert_eq!(format_rate(950.0), "950/s");
        assert_eq!(format_rate(456_000.0), "456k/s");
        assert_eq!(format_rate(1_234_567.0), "1.23M/s");
        assert_eq!(format_rate(f64::NAN), "-/s");
        assert_eq!(format_si(-1.0), "-");
    }

    #[test]
    fn sizes_are_exact_when_they_can_be() {
        assert_eq!(format_size(0), "0B");
        assert_eq!(format_size(1023), "1023B");
        assert_eq!(format_size(1024), "1KiB");
        assert_eq!(format_size(64 << 10), "64KiB");
        assert_eq!(format_size((1 << 20) + (1 << 19)), "1.5MiB");
        assert_eq!(format_size(3 << 30), "3GiB");
        assert_eq!(format_size(5 << 40), "5TiB");
        assert_eq!(format_size(u64::MAX), "16777216.0TiB");
    }

    #[test]
    fn megabytes_per_second_need_bytes_and_time() {
        assert_eq!(format_mb_per_sec(5_000_000, Duration::from_secs(2)), "2.5");
        assert_eq!(format_mb_per_sec(0, Duration::from_secs(1)), "-");
        assert_eq!(format_mb_per_sec(10, Duration::ZERO), "-");
    }
}
