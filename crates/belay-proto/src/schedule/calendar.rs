//! Calendars: named rules that restrict when schedules may fire, such as
//! "weekdays 09:00-17:00, not on public holidays".
//!
//! A calendar works on *local* civil time in its own time zone. The
//! scheduler converts a fire instant to a local date and time (it owns the
//! zone rules) and asks [`CalendarSpec::allows`].

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use belay_common::time::{civil_from_days, days_from_civil, days_in_month, Weekday};
use belay_json::{FromJson, FromJsonError, ObjectBuilder, ObjectReader, ToJson, Value};

use super::validate_timezone;
use crate::error::ParseError;
use crate::ids::SLUG_RULE;
use crate::limits::{MAX_CALENDAR_DATES, MAX_CALENDAR_WINDOWS, MAX_DESCRIPTION_BYTES};
use crate::validate::{Validate, Validator};
use crate::wire::text_json;

/// A date without a time zone, written `YYYY-MM-DD`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CalendarDate {
    year: u16,
    month: u8,
    day: u8,
}

impl CalendarDate {
    /// Validates and builds a date in years 0000-9999.
    pub fn new(year: i32, month: u8, day: u8) -> Result<CalendarDate, ParseError> {
        let shown = || format!("{year:04}-{month:02}-{day:02}");
        if !(0..=9999).contains(&year) {
            return Err(ParseError::invalid(
                "date",
                &shown(),
                "year must be 0000-9999",
            ));
        }
        if !(1..=12).contains(&month) {
            return Err(ParseError::invalid("date", &shown(), "month must be 01-12"));
        }
        let last = days_in_month(year, month);
        if !(1..=last).contains(&day) {
            return Err(ParseError::invalid(
                "date",
                &shown(),
                format!("day must be 01-{last:02} in that month"),
            ));
        }
        Ok(CalendarDate {
            year: year as u16,
            month,
            day,
        })
    }

    /// The year.
    #[must_use]
    pub fn year(self) -> i32 {
        i32::from(self.year)
    }

    /// The month, 1-12.
    #[must_use]
    pub fn month(self) -> u8 {
        self.month
    }

    /// The day of the month, 1-31.
    #[must_use]
    pub fn day(self) -> u8 {
        self.day
    }

    fn days(self) -> i64 {
        days_from_civil(self.year(), self.month, self.day)
    }

    /// The day of the week.
    #[must_use]
    pub fn weekday(self) -> Weekday {
        Weekday::from_days_since_epoch(self.days())
    }

    /// The day before (saturating at 0000-01-01).
    #[must_use]
    pub fn pred(self) -> CalendarDate {
        let (year, month, day) = civil_from_days(self.days() - 1);
        CalendarDate::new(year, month, day).unwrap_or(self)
    }
}

impl fmt::Display for CalendarDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

impl FromStr for CalendarDate {
    type Err = ParseError;

    /// Strictly `YYYY-MM-DD` with zero padding.
    fn from_str(s: &str) -> Result<CalendarDate, ParseError> {
        let b = s.as_bytes();
        let shape_ok = b.len() == 10
            && b[4] == b'-'
            && b[7] == b'-'
            && b.iter()
                .enumerate()
                .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit());
        if !shape_ok {
            return Err(ParseError::invalid("date", s, "expected YYYY-MM-DD"));
        }
        // All-digit slices of at most four characters always parse.
        let num = |range: std::ops::Range<usize>| s[range].parse::<i32>().unwrap_or(0);
        let date = CalendarDate::new(num(0..4), num(5..7) as u8, num(8..10) as u8);
        // Report the error against the text as written.
        date.map_err(|e| ParseError::invalid("date", s, error_reason(&e)))
    }
}

/// The reason part of an `invalid <what> "<input>": <reason>` message.
fn error_reason(e: &ParseError) -> String {
    let text = e.to_string();
    match text.split_once("\": ") {
        Some((_, reason)) => reason.to_owned(),
        None => text,
    }
}

text_json!(CalendarDate, "date");

/// A wall-clock time with minute precision, written `HH:MM`. `24:00` is
/// accepted as the end of a window that runs to midnight.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimeOfDay {
    minutes: u16,
}

impl TimeOfDay {
    /// Midnight at the start of the day.
    pub const MIDNIGHT: TimeOfDay = TimeOfDay { minutes: 0 };
    /// `24:00`, the end of the day.
    pub const END_OF_DAY: TimeOfDay = TimeOfDay { minutes: 24 * 60 };

    /// Builds `hour:minute`; hour 24 is only valid with minute 0.
    pub fn new(hour: u8, minute: u8) -> Result<TimeOfDay, ParseError> {
        let minutes = u16::from(hour) * 60 + u16::from(minute);
        if minute > 59 || minutes > 24 * 60 {
            let shown = format!("{hour:02}:{minute:02}");
            return Err(ParseError::invalid(
                "time of day",
                &shown,
                "expected 00:00 through 24:00",
            ));
        }
        Ok(TimeOfDay { minutes })
    }

    /// Minutes since midnight, `0..=1440`.
    #[must_use]
    pub fn minutes(self) -> u16 {
        self.minutes
    }
}

impl fmt::Display for TimeOfDay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}", self.minutes / 60, self.minutes % 60)
    }
}

impl FromStr for TimeOfDay {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<TimeOfDay, ParseError> {
        let b = s.as_bytes();
        let shape_ok =
            b.len() == 5 && b[2] == b':' && [0, 1, 3, 4].iter().all(|&i| b[i].is_ascii_digit());
        if !shape_ok {
            return Err(ParseError::invalid("time of day", s, "expected HH:MM"));
        }
        let hour = (b[0] - b'0') * 10 + (b[1] - b'0');
        let minute = (b[3] - b'0') * 10 + (b[4] - b'0');
        TimeOfDay::new(hour, minute)
    }
}

text_json!(TimeOfDay, "time of day");

/// A set of weekdays.
///
/// Text form: comma-separated days or ranges, `mon-fri`, `sat,sun`,
/// `fri-mon` (ranges may wrap). Names are full or three-letter, any case.
/// JSON: an array of lowercase three-letter names in ISO order (Monday
/// first); on input the array items may themselves be ranges, and a single
/// string is accepted too.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct WeekdaySet(u8);

impl WeekdaySet {
    /// No days.
    pub const EMPTY: WeekdaySet = WeekdaySet(0);
    /// Every day.
    pub const ALL: WeekdaySet = WeekdaySet(0x7F);

    fn bit(day: Weekday) -> u8 {
        1 << (day.number_from_monday() - 1)
    }

    /// Adds a day.
    pub fn insert(&mut self, day: Weekday) {
        self.0 |= WeekdaySet::bit(day);
    }

    /// Whether `day` is in the set.
    #[must_use]
    pub fn contains(self, day: Weekday) -> bool {
        self.0 & WeekdaySet::bit(day) != 0
    }

    /// Whether the set has no days.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Number of days in the set.
    #[must_use]
    pub fn len(self) -> usize {
        self.0.count_ones() as usize
    }

    /// The days, Monday first.
    pub fn iter(self) -> impl Iterator<Item = Weekday> {
        Weekday::ALL.into_iter().filter(move |d| self.contains(*d))
    }

    fn parse_day(s: &str, whole: &str) -> Result<Weekday, ParseError> {
        s.trim().parse::<Weekday>().map_err(|_| {
            ParseError::invalid(
                "weekday set",
                whole,
                format!(
                    "{:?} is not a weekday (use mon, tue, ... or monday, ...)",
                    s.trim()
                ),
            )
        })
    }

    fn add_item(&mut self, item: &str, whole: &str) -> Result<(), ParseError> {
        match item.split_once('-') {
            Some((from, to)) => {
                let (mut day, last) = (Self::parse_day(from, whole)?, Self::parse_day(to, whole)?);
                self.insert(day);
                while day != last {
                    day = day.succ();
                    self.insert(day);
                }
            }
            None => self.insert(Self::parse_day(item, whole)?),
        }
        Ok(())
    }
}

impl FromIterator<Weekday> for WeekdaySet {
    fn from_iter<I: IntoIterator<Item = Weekday>>(days: I) -> WeekdaySet {
        let mut set = WeekdaySet::EMPTY;
        for day in days {
            set.insert(day);
        }
        set
    }
}

impl fmt::Display for WeekdaySet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<String> = self.iter().map(short_name).collect();
        f.write_str(&names.join(","))
    }
}

impl FromStr for WeekdaySet {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<WeekdaySet, ParseError> {
        let mut set = WeekdaySet::EMPTY;
        for item in s.split(',') {
            set.add_item(item, s)?;
        }
        Ok(set)
    }
}

fn short_name(day: Weekday) -> String {
    day.short_name().to_ascii_lowercase()
}

impl ToJson for WeekdaySet {
    fn to_json(&self) -> Value {
        Value::Array(self.iter().map(|d| Value::String(short_name(d))).collect())
    }
}
