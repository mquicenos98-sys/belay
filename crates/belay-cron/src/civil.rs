//! Proleptic Gregorian calendar arithmetic and the [`LocalDateTime`] type.
//!
//! Day numbers count days since 1970-01-01 using Howard Hinnant's
//! `days_from_civil` / `civil_from_days` algorithms
//! (<https://howardhinnant.github.io/date_algorithms.html>): exact, table-free
//! and branch-light. "Naive" seconds are seconds since 1970-01-01T00:00:00 on a
//! wall clock, with no time zone attached; a time zone turns them into Unix
//! seconds by subtracting its UTC offset. Weekdays are numbered 0 = Sunday, as
//! in cron.

use std::fmt;
use std::str::FromStr;

use crate::error::{CronError, Result};

pub(crate) const SECS_PER_DAY: i64 = 86_400;

/// Naive seconds of 0001-01-01T00:00:00, the earliest representable reading.
pub(crate) const MIN_NAIVE: i64 = -62_135_596_800;

/// Naive seconds of 9999-12-31T23:59:59, the latest representable reading.
pub(crate) const MAX_NAIVE: i64 = 253_402_300_799;

pub(crate) const fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

pub(crate) const fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date. `month` is 1-12 and
/// `day` 1-31; out-of-range days roll over arithmetically, which callers use
/// on purpose for "the 0th of March" style computations.
pub(crate) const fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = month as i64;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`]: `(year, month, day)`.
pub(crate) const fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}
