//! Proleptic Gregorian calendar arithmetic.
//!
//! The day-count conversions are Howard Hinnant's `days_from_civil` and
//! `civil_from_days` (<https://howardhinnant.github.io/date_algorithms.html>):
//! branch-light, exact over the whole `i32` year range, and they treat the
//! year as starting on March 1st so that the leap day is the last day of the
//! internal year.

use std::fmt;
use std::str::FromStr;

use super::{TimeError, Timestamp};

/// Days in a 400-year Gregorian cycle.
const DAYS_PER_ERA: i64 = 146_097;

/// Days from 0000-03-01 to 1970-01-01.
const EPOCH_SHIFT: i64 = 719_468;

/// Whether `year` is a Gregorian leap year.
#[must_use]
pub const fn is_leap_year(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// Number of days in `month` (1-12) of `year`; 0 for an invalid month.
#[must_use]
pub const fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days since 1970-01-01 of the given date. Fields are not validated; an
/// out-of-range day simply counts past the end of the month.
#[must_use]
pub const fn days_from_civil(year: i32, month: u8, day: u8) -> i64 {
    let y = year as i64 - if month <= 2 { 1 } else { 0 };
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // [0, 399]
    let m = month as i64;
    let shifted_month = if m > 2 { m - 3 } else { m + 9 }; // March = 0
    let doy = (153 * shifted_month + 2) / 5 + day as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * DAYS_PER_ERA + doe - EPOCH_SHIFT
}

/// The date `(year, month, day)` that is `days` days after 1970-01-01.
#[must_use]
pub const fn civil_from_days(days: i64) -> (i32, u8, u8) {
    let z = days + EPOCH_SHIFT;
    let era = z.div_euclid(DAYS_PER_ERA);
    let doe = z - era * DAYS_PER_ERA; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March = 0
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    (year as i32, month as u8, day as u8)
}

/// A day of the week.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Weekday {
    /// Monday.
    Monday,
    /// Tuesday.
    Tuesday,
    /// Wednesday.
    Wednesday,
    /// Thursday.
    Thursday,
    /// Friday.
    Friday,
    /// Saturday.
    Saturday,
    /// Sunday.
    Sunday,
}

impl Weekday {
    /// All weekdays, Monday first (ISO 8601 order).
    pub const ALL: [Weekday; 7] = [
        Weekday::Monday,
        Weekday::Tuesday,
        Weekday::Wednesday,
        Weekday::Thursday,
        Weekday::Friday,
        Weekday::Saturday,
        Weekday::Sunday,
    ];

    /// The weekday of the day that is `days` days after 1970-01-01 (a Thursday).
    #[must_use]
    pub const fn from_days_since_epoch(days: i64) -> Weekday {
        Weekday::ALL[(days + 3).rem_euclid(7) as usize]
    }

    /// ISO 8601 number: Monday = 1 through Sunday = 7.
    #[must_use]
    pub const fn number_from_monday(self) -> u8 {
        self as u8 + 1
    }

    /// Days since the preceding Sunday: Sunday = 0 through Saturday = 6 (the
    /// numbering used by cron).
    #[must_use]
    pub const fn days_from_sunday(self) -> u8 {
        (self as u8 + 1) % 7
    }

    /// The following day.
    #[must_use]
    pub const fn succ(self) -> Weekday {
        Weekday::ALL[(self as usize + 1) % 7]
    }

    /// The preceding day.
    #[must_use]
    pub const fn pred(self) -> Weekday {
        Weekday::ALL[(self as usize + 6) % 7]
    }

    /// English name, e.g. `"Monday"`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Weekday::Monday => "Monday",
            Weekday::Tuesday => "Tuesday",
            Weekday::Wednesday => "Wednesday",
            Weekday::Thursday => "Thursday",
            Weekday::Friday => "Friday",
            Weekday::Saturday => "Saturday",
            Weekday::Sunday => "Sunday",
        }
    }

    /// Three-letter abbreviation, e.g. `"Mon"`.
    #[must_use]
    pub fn short_name(self) -> &'static str {
        &self.name()[..3]
    }

    /// Whether this is Saturday or Sunday.
    #[must_use]
    pub const fn is_weekend(self) -> bool {
        matches!(self, Weekday::Saturday | Weekday::Sunday)
    }
}

impl fmt::Display for Weekday {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Accepts full names and three-letter abbreviations, case-insensitively.
impl FromStr for Weekday {
    type Err = TimeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Weekday::ALL
            .into_iter()
            .find(|d| s.eq_ignore_ascii_case(d.name()) || s.eq_ignore_ascii_case(d.short_name()))
            .ok_or_else(|| TimeError::InvalidWeekday(s.to_owned()))
    }
}
