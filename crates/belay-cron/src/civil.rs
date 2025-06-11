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

/// Weekday of a day number, 0 = Sunday (1970-01-01 was a Thursday).
pub(crate) const fn weekday_from_days(days: i64) -> u32 {
    (days + 4).rem_euclid(7) as u32
}

/// A day of the week.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Weekday {
    /// Day 0 (and 7) in cron.
    Sunday,
    /// Day 1.
    Monday,
    /// Day 2.
    Tuesday,
    /// Day 3.
    Wednesday,
    /// Day 4.
    Thursday,
    /// Day 5.
    Friday,
    /// Day 6.
    Saturday,
}

impl Weekday {
    /// All weekdays, Sunday first.
    pub const ALL: [Weekday; 7] = [
        Weekday::Sunday,
        Weekday::Monday,
        Weekday::Tuesday,
        Weekday::Wednesday,
        Weekday::Thursday,
        Weekday::Friday,
        Weekday::Saturday,
    ];

    /// The weekday with cron number `index` (taken modulo 7, so 7 is Sunday).
    #[must_use]
    pub const fn from_index(index: u32) -> Weekday {
        Weekday::ALL[(index % 7) as usize]
    }

    /// Cron number of the weekday: 0 = Sunday … 6 = Saturday.
    #[must_use]
    pub const fn index(self) -> u32 {
        self as u32
    }

    /// Full English name, e.g. `"Monday"`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Weekday::Sunday => "Sunday",
            Weekday::Monday => "Monday",
            Weekday::Tuesday => "Tuesday",
            Weekday::Wednesday => "Wednesday",
            Weekday::Thursday => "Thursday",
            Weekday::Friday => "Friday",
            Weekday::Saturday => "Saturday",
        }
    }

    /// Three-letter uppercase abbreviation as used in cron, e.g. `"MON"`.
    #[must_use]
    pub const fn abbreviation(self) -> &'static str {
        match self {
            Weekday::Sunday => "SUN",
            Weekday::Monday => "MON",
            Weekday::Tuesday => "TUE",
            Weekday::Wednesday => "WED",
            Weekday::Thursday => "THU",
            Weekday::Friday => "FRI",
            Weekday::Saturday => "SAT",
        }
    }
}
