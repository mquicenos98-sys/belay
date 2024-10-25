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

/// A timestamp broken down into UTC calendar fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Civil {
    /// Year, 0 through 9999.
    pub year: i32,
    /// Month, 1 through 12.
    pub month: u8,
    /// Day of the month, 1 through 31.
    pub day: u8,
    /// Hour, 0 through 23.
    pub hour: u8,
    /// Minute, 0 through 59.
    pub minute: u8,
    /// Second, 0 through 59.
    pub second: u8,
    /// Millisecond, 0 through 999.
    pub millis: u16,
    /// Day of the week.
    pub weekday: Weekday,
}

impl Civil {
    /// Day of the year, 1 through 366.
    #[must_use]
    pub fn day_of_year(&self) -> u16 {
        let days =
            days_from_civil(self.year, self.month, self.day) - days_from_civil(self.year, 1, 1);
        days as u16 + 1
    }

    /// Converts back to a timestamp. `weekday` is ignored; the other fields
    /// are validated as in [`Timestamp::from_civil`].
    pub fn to_timestamp(&self) -> Result<Timestamp, TimeError> {
        Timestamp::from_civil(
            self.year,
            self.month,
            self.day,
            self.hour,
            self.minute,
            self.second,
            self.millis,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leap_year_rules_cover_centuries() {
        assert!(is_leap_year(2024));
        assert!(!is_leap_year(2023));
        assert!(!is_leap_year(1900));
        assert!(!is_leap_year(2100));
        assert!(is_leap_year(2000));
        assert!(is_leap_year(1600));
        assert!(is_leap_year(0));
    }

    #[test]
    fn days_in_month_handles_february_and_invalid_months() {
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(1900, 2), 28);
        assert_eq!(days_in_month(2023, 4), 30);
        assert_eq!(days_in_month(2023, 12), 31);
        assert_eq!(days_in_month(2023, 0), 0);
        assert_eq!(days_in_month(2023, 13), 0);
    }

    #[test]
    fn day_counts_match_known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(0, 1, 1), -719_528);
        assert_eq!(days_from_civil(9999, 12, 31), 2_932_896);
        assert_eq!(civil_from_days(-719_528), (0, 1, 1));
        assert_eq!(civil_from_days(2_932_896), (9999, 12, 31));
    }

    #[test]
    fn civil_day_conversion_round_trips_over_whole_range() {
        // Walk every 7th day from year 0 to 9999 plus both neighbours of each
        // sample; checks the inverse relation and that dates are consecutive.
        let start = days_from_civil(0, 1, 1);
        let end = days_from_civil(9999, 12, 31);
        let mut day = start;
        while day <= end {
            let (y, m, d) = civil_from_days(day);
            assert!((1..=12).contains(&m) && d >= 1 && d <= days_in_month(y, m));
            assert_eq!(days_from_civil(y, m, d), day);
            let (ny, nm, nd) = civil_from_days(day + 1);
            let expected_next = if d < days_in_month(y, m) {
                (y, m, d + 1)
            } else if m < 12 {
                (y, m + 1, 1)
            } else {
                (y + 1, 1, 1)
            };
            assert_eq!((ny, nm, nd), expected_next);
            day += 7;
        }
    }

    #[test]
    fn weekday_numbering_schemes() {
        assert_eq!(Weekday::from_days_since_epoch(0), Weekday::Thursday);
        assert_eq!(Weekday::from_days_since_epoch(-1), Weekday::Wednesday);
        assert_eq!(Weekday::from_days_since_epoch(-4), Weekday::Sunday);
        assert_eq!(Weekday::Monday.number_from_monday(), 1);
        assert_eq!(Weekday::Sunday.number_from_monday(), 7);
        assert_eq!(Weekday::Sunday.days_from_sunday(), 0);
        assert_eq!(Weekday::Saturday.days_from_sunday(), 6);
        assert_eq!(Weekday::Sunday.succ(), Weekday::Monday);
        assert_eq!(Weekday::Monday.pred(), Weekday::Sunday);
        assert!(Weekday::Saturday.is_weekend() && !Weekday::Friday.is_weekend());
    }

    #[test]
    fn weekday_parses_full_and_short_names() {
        assert_eq!("monday".parse::<Weekday>(), Ok(Weekday::Monday));
        assert_eq!("SAT".parse::<Weekday>(), Ok(Weekday::Saturday));
        assert_eq!("Thu".parse::<Weekday>(), Ok(Weekday::Thursday));
        let err = "Mo".parse::<Weekday>().unwrap_err();
        assert_eq!(err.to_string(), "unknown weekday \"Mo\"");
        assert_eq!(Weekday::Wednesday.to_string(), "Wednesday");
        assert_eq!(Weekday::Wednesday.short_name(), "Wed");
    }

    #[test]
    fn day_of_year_counts_leap_days() {
        let c = Timestamp::from_date(2024, 12, 31).unwrap().to_civil();
        assert_eq!(c.day_of_year(), 366);
        let c = Timestamp::from_date(2023, 3, 1).unwrap().to_civil();
        assert_eq!(c.day_of_year(), 60);
        let c = Timestamp::from_date(2023, 1, 1).unwrap().to_civil();
        assert_eq!(c.day_of_year(), 1);
    }
}
