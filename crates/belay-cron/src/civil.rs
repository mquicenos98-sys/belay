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

impl fmt::Display for Weekday {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A wall-clock date and time with one-second resolution and no time zone.
///
/// Ordering is chronological. Years 1 through 9999 are representable.
///
/// ```
/// use belay_cron::LocalDateTime;
///
/// let t: LocalDateTime = "2025-03-09T02:30:00".parse()?;
/// assert_eq!((t.year(), t.month(), t.day()), (2025, 3, 9));
/// assert_eq!(t.weekday().name(), "Sunday");
/// assert_eq!(t.to_string(), "2025-03-09T02:30:00");
/// # Ok::<(), belay_cron::CronError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalDateTime {
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
}

impl LocalDateTime {
    /// Builds a date-time, returning `None` if any component is out of range
    /// (including February 29 in a common year).
    #[must_use]
    pub fn new(
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
        second: u32,
    ) -> Option<LocalDateTime> {
        let valid = (1..=9999).contains(&year)
            && (1..=12).contains(&month)
            && day >= 1
            && day <= days_in_month(i64::from(year), month)
            && hour < 24
            && minute < 60
            && second < 60;
        valid.then(|| LocalDateTime {
            year,
            month: month as u8,
            day: day as u8,
            hour: hour as u8,
            minute: minute as u8,
            second: second as u8,
        })
    }

    /// Midnight at the start of the given date.
    #[must_use]
    pub fn from_ymd(year: i32, month: u32, day: u32) -> Option<LocalDateTime> {
        LocalDateTime::new(year, month, day, 0, 0, 0)
    }

    /// Parses `YYYY-MM-DDTHH:MM[:SS]`; a space may replace the `T`, and a
    /// bare `YYYY-MM-DD` means midnight.
    pub fn parse(input: &str) -> Result<LocalDateTime> {
        let fail = |reason| CronError::InvalidDateTime {
            input: input.to_string(),
            reason,
        };
        let s = input.trim();
        let (date, time) = match s.find(['T', 't', ' ']) {
            Some(i) => (&s[..i], Some(&s[i + 1..])),
            None => (s, None),
        };
        let mut date_parts = date.split('-');
        let year = parse_component(date_parts.next(), 4)
            .ok_or_else(|| fail("expected a four-digit year"))?;
        let month = parse_component(date_parts.next(), 2)
            .ok_or_else(|| fail("expected a two-digit month"))?;
        let day = parse_component(date_parts.next(), 2)
            .ok_or_else(|| fail("expected a two-digit day"))?;
        if date_parts.next().is_some() {
            return Err(fail("unexpected text after the date"));
        }
        let (hour, minute, second) = match time {
            None => (0, 0, 0),
            Some(time) => {
                let mut parts = time.split(':');
                let hour = parse_component(parts.next(), 2)
                    .ok_or_else(|| fail("expected a two-digit hour"))?;
                let minute = parse_component(parts.next(), 2)
                    .ok_or_else(|| fail("expected a two-digit minute"))?;
                let second = match parts.next() {
                    None => 0,
                    Some(p) => parse_component(Some(p), 2)
                        .ok_or_else(|| fail("expected a two-digit second"))?,
                };
                if parts.next().is_some() {
                    return Err(fail("unexpected text after the time"));
                }
                (hour, minute, second)
            }
        };
        if !(1..=12).contains(&month) {
            return Err(fail("month out of range"));
        }
        if day == 0 || day > days_in_month(i64::from(year), month) {
            return Err(fail("day out of range for month"));
        }
        LocalDateTime::new(year as i32, month, day, hour, minute, second)
            .ok_or_else(|| fail("time of day out of range"))
    }

    /// Calendar year.
    #[must_use]
    pub const fn year(&self) -> i32 {
        self.year
    }

    /// Month, 1-12.
    #[must_use]
    pub const fn month(&self) -> u32 {
        self.month as u32
    }

    /// Day of the month, 1-31.
    #[must_use]
    pub const fn day(&self) -> u32 {
        self.day as u32
    }

    /// Hour, 0-23.
    #[must_use]
    pub const fn hour(&self) -> u32 {
        self.hour as u32
    }

    /// Minute, 0-59.
    #[must_use]
    pub const fn minute(&self) -> u32 {
        self.minute as u32
    }

    /// Second, 0-59.
    #[must_use]
    pub const fn second(&self) -> u32 {
        self.second as u32
    }

    /// Day of the week.
    #[must_use]
    pub fn weekday(&self) -> Weekday {
        Weekday::from_index(weekday_from_days(self.days()))
    }

    /// Day of the year, 1-366.
    #[must_use]
    pub fn ordinal(&self) -> u32 {
        (self.days() - days_from_civil(i64::from(self.year), 1, 1)) as u32 + 1
    }

    fn days(&self) -> i64 {
        days_from_civil(i64::from(self.year), self.month(), self.day())
    }

    /// Seconds since 1970-01-01T00:00:00 on the same wall clock.
    pub(crate) fn to_naive(self) -> i64 {
        self.days() * SECS_PER_DAY
            + i64::from(self.hour) * 3600
            + i64::from(self.minute) * 60
            + i64::from(self.second)
    }

    /// Inverse of [`to_naive`](Self::to_naive), clamped to years 1-9999.
    pub(crate) fn from_naive(naive: i64) -> LocalDateTime {
        let naive = naive.clamp(MIN_NAIVE, MAX_NAIVE);
        let (year, month, day) = civil_from_days(naive.div_euclid(SECS_PER_DAY));
        let sod = naive.rem_euclid(SECS_PER_DAY);
        LocalDateTime {
            year: year as i32,
            month: month as u8,
            day: day as u8,
            hour: (sod / 3600) as u8,
            minute: (sod / 60 % 60) as u8,
            second: (sod % 60) as u8,
        }
    }
}

fn parse_component(part: Option<&str>, digits: usize) -> Option<u32> {
    let part = part?;
    if part.len() != digits || !part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    part.parse().ok()
}

impl fmt::Display for LocalDateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

impl FromStr for LocalDateTime {
    type Err = CronError;

    fn from_str(s: &str) -> Result<Self> {
        LocalDateTime::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_from_civil_matches_known_epochs() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 1, 1) * SECS_PER_DAY, 946_684_800);
        assert_eq!(days_from_civil(2025, 1, 1) * SECS_PER_DAY, 1_735_689_600);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(days_from_civil(1, 1, 1) * SECS_PER_DAY, MIN_NAIVE);
        assert_eq!(
            days_from_civil(9999, 12, 31) * SECS_PER_DAY + SECS_PER_DAY - 1,
            MAX_NAIVE
        );
    }

    #[test]
    fn civil_round_trips_over_four_centuries() {
        // Walk every day across a full 400-year Gregorian cycle and some
        // negative day numbers; each must round-trip and advance by one day.
        let start = days_from_civil(1899, 12, 25);
        let mut prev = civil_from_days(start - 1);
        for days in start..start + 146_097 + 10 {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
            assert!(d >= 1 && d <= days_in_month(y, m));
            if d == 1 {
                assert_eq!(prev.2, days_in_month(prev.0, prev.1));
            } else {
                assert_eq!((prev.0, prev.1, prev.2 + 1), (y, m, d));
            }
            prev = (y, m, d);
        }
    }

    #[test]
    fn leap_years_follow_gregorian_rules() {
        assert!(is_leap_year(2000));
        assert!(is_leap_year(2024));
        assert!(!is_leap_year(1900));
        assert!(!is_leap_year(2100));
        assert!(!is_leap_year(2025));
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2100, 2), 28);
        assert_eq!(days_in_month(2025, 4), 30);
    }

    #[test]
    fn weekdays_match_known_dates() {
        assert_eq!(weekday_from_days(0), 4); // 1970-01-01, Thursday
        assert_eq!(weekday_from_days(days_from_civil(2025, 3, 9)), 0);
        assert_eq!(weekday_from_days(days_from_civil(2000, 1, 1)), 6);
        assert_eq!(weekday_from_days(days_from_civil(1969, 12, 28)), 0);
        assert_eq!(Weekday::from_index(7), Weekday::Sunday);
        assert_eq!(Weekday::Friday.abbreviation(), "FRI");
    }

    #[test]
    fn local_date_time_rejects_impossible_dates() {
        assert!(LocalDateTime::new(2025, 2, 29, 0, 0, 0).is_none());
        assert!(LocalDateTime::new(2024, 2, 29, 0, 0, 0).is_some());
        assert!(LocalDateTime::new(2025, 13, 1, 0, 0, 0).is_none());
        assert!(LocalDateTime::new(2025, 1, 1, 24, 0, 0).is_none());
        assert!(LocalDateTime::new(0, 1, 1, 0, 0, 0).is_none());
    }

    #[test]
    fn local_date_time_parses_common_forms() {
        let t = LocalDateTime::parse("2025-11-02 01:30").unwrap();
        assert_eq!(t, LocalDateTime::new(2025, 11, 2, 1, 30, 0).unwrap());
        let t = LocalDateTime::parse("2024-02-29").unwrap();
        assert_eq!(t.to_string(), "2024-02-29T00:00:00");
        assert_eq!(t.ordinal(), 60);
        let err = LocalDateTime::parse("2025-02-29T00:00").unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid local date-time \"2025-02-29T00:00\": day out of range for month"
        );
        assert!(LocalDateTime::parse("2025-1-01").is_err());
        assert!(LocalDateTime::parse("2025-01-01T25:00").is_err());
        assert!(LocalDateTime::parse("2025-01-01T10:00:00:00").is_err());
        assert!(LocalDateTime::parse("").is_err());
    }

    #[test]
    fn naive_seconds_round_trip_and_clamp() {
        let t = LocalDateTime::new(2025, 3, 9, 2, 30, 15).unwrap();
        assert_eq!(LocalDateTime::from_naive(t.to_naive()), t);
        assert_eq!(
            LocalDateTime::from_naive(i64::MAX).to_string(),
            "9999-12-31T23:59:59"
        );
        assert_eq!(
            LocalDateTime::from_naive(i64::MIN).to_string(),
            "0001-01-01T00:00:00"
        );
        assert_eq!(
            LocalDateTime::from_naive(-1).to_string(),
            "1969-12-31T23:59:59"
        );
    }

    #[test]
    fn ordering_is_chronological() {
        let a = LocalDateTime::new(2025, 1, 31, 23, 59, 59).unwrap();
        let b = LocalDateTime::new(2025, 2, 1, 0, 0, 0).unwrap();
        assert!(a < b);
        assert!(a.to_naive() < b.to_naive());
    }
}
