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

impl FromJson for WeekdaySet {
    fn from_json(value: &Value) -> Result<WeekdaySet, FromJsonError> {
        let parse = |s: &str| {
            s.parse::<WeekdaySet>()
                .map_err(|e| FromJsonError::new(e.to_string()))
        };
        match value {
            Value::String(s) => parse(s),
            Value::Array(items) => {
                let mut set = WeekdaySet::EMPTY;
                for (i, item) in items.iter().enumerate() {
                    let s = item.as_str().ok_or_else(|| {
                        FromJsonError::expected("weekday string", item).at_index(i)
                    })?;
                    set.0 |= parse(s).map_err(|e| e.at_index(i))?.0;
                }
                Ok(set)
            }
            other => Err(FromJsonError::expected("array of weekdays", other)),
        }
    }
}

/// A daily time range on some weekdays, `[start, end)` in local time.
///
/// A window whose `end` is not after its `start` wraps past midnight:
/// `22:00-06:00` on `fri` covers Friday 22:00 to Saturday 06:00. `days`
/// always names the day the window *starts*; an empty set means every day.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeWindow {
    /// The days the window starts on; empty for every day.
    pub days: WeekdaySet,
    /// Start, inclusive.
    pub start: TimeOfDay,
    /// End, exclusive.
    pub end: TimeOfDay,
}

impl TimeWindow {
    /// A window on `days` from `start` to `end`.
    #[must_use]
    pub fn new(days: WeekdaySet, start: TimeOfDay, end: TimeOfDay) -> TimeWindow {
        TimeWindow { days, start, end }
    }

    fn starts_on(&self, day: Weekday) -> bool {
        self.days.is_empty() || self.days.contains(day)
    }

    /// Whether local time `time` on `date` falls inside the window.
    #[must_use]
    pub fn contains(&self, date: CalendarDate, time: TimeOfDay) -> bool {
        if self.start < self.end {
            self.starts_on(date.weekday()) && self.start <= time && time < self.end
        } else {
            (self.starts_on(date.weekday()) && time >= self.start)
                || (self.starts_on(date.weekday()) && time < self.end)
        }
    }
}

impl ToJson for TimeWindow {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field_if(!self.days.is_empty(), "days", self.days)
            .field("start", self.start)
            .field("end", self.end)
            .build()
    }
}

impl FromJson for TimeWindow {
    fn from_json(value: &Value) -> Result<TimeWindow, FromJsonError> {
        let r = ObjectReader::new(value, "TimeWindow")?;
        let window = TimeWindow {
            days: r.or_default("days")?,
            start: r.req("start")?,
            end: r.req("end")?,
        };
        r.deny_unknown_fields()?;
        Ok(window)
    }
}

impl Validate for TimeWindow {
    fn validate_into(&self, v: &mut Validator) {
        v.check(self.start != TimeOfDay::END_OF_DAY, "start", || {
            "24:00 is only valid as an end time".to_owned()
        });
        v.check(self.start != self.end, "end", || {
            "must differ from start; use 00:00-24:00 for the whole day".to_owned()
        });
    }
}

/// A named set of rules for when schedules may fire.
///
/// A local date and time is allowed when its date is not excluded, its
/// weekday is not excluded, and it falls inside at least one window (or
/// there are no windows).
///
/// ```
/// use belay_proto::schedule::{CalendarDate, CalendarSpec, TimeOfDay};
///
/// let cal: CalendarSpec = belay_json::from_str(r#"{
///     "name": "business-hours",
///     "timezone": "America/Bogota",
///     "exclude_dates": ["2025-12-25"],
///     "windows": [{"days": ["mon-fri"], "start": "09:00", "end": "17:00"}]
/// }"#).unwrap();
/// let tuesday: CalendarDate = "2025-12-23".parse().unwrap();
/// assert!(cal.allows(tuesday, "10:30".parse().unwrap()));
/// assert!(!cal.allows(tuesday, "17:00".parse().unwrap()));
/// assert!(!cal.allows("2025-12-25".parse().unwrap(), "10:30".parse().unwrap()));
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CalendarSpec {
    /// The calendar's name, referenced by schedules.
    pub name: String,
    /// Zone of the local times the rules refer to; default `UTC`.
    pub timezone: String,
    /// Dates on which nothing fires.
    pub exclude_dates: BTreeSet<CalendarDate>,
    /// Weekdays on which nothing fires.
    pub exclude_weekdays: WeekdaySet,
    /// Allowed time ranges; empty for the whole day.
    pub windows: Vec<TimeWindow>,
    /// Free text for humans.
    pub description: Option<String>,
}

impl CalendarSpec {
    /// A calendar that allows everything, in UTC.
    #[must_use]
    pub fn new(name: impl Into<String>) -> CalendarSpec {
        CalendarSpec {
            name: name.into(),
            timezone: "UTC".to_owned(),
            exclude_dates: BTreeSet::new(),
            exclude_weekdays: WeekdaySet::EMPTY,
            windows: Vec::new(),
            description: None,
        }
    }

    /// Whether local time `time` on `date` is allowed.
    #[must_use]
    pub fn allows(&self, date: CalendarDate, time: TimeOfDay) -> bool {
        !self.exclude_dates.contains(&date)
            && !self.exclude_weekdays.contains(date.weekday())
            && (self.windows.is_empty() || self.windows.iter().any(|w| w.contains(date, time)))
    }
}

impl ToJson for CalendarSpec {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("name", &self.name)
            .field("timezone", &self.timezone)
            .field_if(
                !self.exclude_dates.is_empty(),
                "exclude_dates",
                &self.exclude_dates,
            )
            .field_if(
                !self.exclude_weekdays.is_empty(),
                "exclude_weekdays",
                self.exclude_weekdays,
            )
            .field_if(!self.windows.is_empty(), "windows", &self.windows)
            .opt_field("description", &self.description)
            .build()
    }
}

impl FromJson for CalendarSpec {
    fn from_json(value: &Value) -> Result<CalendarSpec, FromJsonError> {
        let r = ObjectReader::new(value, "CalendarSpec")?;
        let spec = CalendarSpec {
            name: r.req("name")?,
            timezone: r.or_else("timezone", || "UTC".to_owned())?,
            exclude_dates: r.or_default("exclude_dates")?,
            exclude_weekdays: r.or_default("exclude_weekdays")?,
            windows: r.or_default("windows")?,
            description: r.opt("description")?,
        };
        r.deny_unknown_fields()?;
        Ok(spec)
    }
}

impl Validate for CalendarSpec {
    fn validate_into(&self, v: &mut Validator) {
        v.check_name("name", &self.name, SLUG_RULE);
        validate_timezone(v, &self.timezone);
        v.check(
            self.exclude_dates.len() <= MAX_CALENDAR_DATES,
            "exclude_dates",
            || {
                format!(
                    "at most {MAX_CALENDAR_DATES} dates are allowed, got {}",
                    self.exclude_dates.len()
                )
            },
        );
        v.check(
            self.exclude_weekdays != WeekdaySet::ALL,
            "exclude_weekdays",
            || "excludes every day, so nothing could ever fire".to_owned(),
        );
        v.check(
            self.windows.len() <= MAX_CALENDAR_WINDOWS,
            "windows",
            || {
                format!(
                    "at most {MAX_CALENDAR_WINDOWS} windows are allowed, got {}",
                    self.windows.len()
                )
            },
        );
        v.nested("windows", &self.windows);
        if let Some(text) = &self.description {
            v.check_bytes("description", text, MAX_DESCRIPTION_BYTES);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_json::json;

    fn date(s: &str) -> CalendarDate {
        s.parse().unwrap()
    }

    fn time(s: &str) -> TimeOfDay {
        s.parse().unwrap()
    }

    #[test]
    fn dates_are_strict_and_know_their_weekday() {
        assert_eq!(date("2024-02-29").weekday(), Weekday::Thursday);
        assert_eq!(date("2025-03-01").pred(), date("2025-02-28"));
        assert_eq!(date("2024-03-01").pred(), date("2024-02-29"));
        assert_eq!(date("0000-01-01").pred(), date("0000-01-01"));
        assert_eq!(date("2025-01-05").to_string(), "2025-01-05");
        for (bad, reason) in [
            ("2025-1-5", "expected YYYY-MM-DD"),
            ("2025/01/05", "expected YYYY-MM-DD"),
            ("2023-02-29", "day must be 01-28 in that month"),
            ("2023-13-01", "month must be 01-12"),
        ] {
            assert_eq!(
                bad.parse::<CalendarDate>().unwrap_err().to_string(),
                format!("invalid date {bad:?}: {reason}")
            );
        }
        assert!(date("2025-01-31") > date("2025-01-30"));
    }

    #[test]
    fn times_of_day_allow_end_of_day_only_on_the_hour() {
        assert_eq!(time("00:00"), TimeOfDay::MIDNIGHT);
        assert_eq!(time("24:00"), TimeOfDay::END_OF_DAY);
        assert_eq!(time("09:05").minutes(), 545);
        assert_eq!(
            "24:01".parse::<TimeOfDay>().unwrap_err().to_string(),
            r#"invalid time of day "24:01": expected 00:00 through 24:00"#
        );
        assert!("9:00".parse::<TimeOfDay>().is_err());
        assert!("09:60".parse::<TimeOfDay>().is_err());
    }

    #[test]
    fn weekday_sets_parse_ranges_and_wraps() {
        let weekdays: WeekdaySet = "mon-fri".parse().unwrap();
        assert_eq!(weekdays.len(), 5);
        assert_eq!(weekdays.to_string(), "mon,tue,wed,thu,fri");
        let wrap: WeekdaySet = "fri-mon".parse().unwrap();
        assert_eq!(wrap.to_string(), "mon,fri,sat,sun");
        let named: WeekdaySet = "Saturday, SUN".parse().unwrap();
        assert_eq!(named.to_json(), json!(["sat", "sun"]));
        assert_eq!(
            "mon-fry".parse::<WeekdaySet>().unwrap_err().to_string(),
            r#"invalid weekday set "mon-fry": "fry" is not a weekday (use mon, tue, ... or monday, ...)"#
        );
    }

    #[test]
    fn weekday_set_json_accepts_arrays_of_ranges_and_strings() {
        let a = WeekdaySet::from_json(&json!(["mon-wed", "fri"])).unwrap();
        assert_eq!(a.to_string(), "mon,tue,wed,fri");
        assert_eq!(WeekdaySet::from_json(&json!("sat,sun")).unwrap().len(), 2);
        let err = WeekdaySet::from_json(&json!(["mon", 3])).unwrap_err();
        assert_eq!(err.to_string(), "$[1]: expected weekday string, found 3");
        let err = WeekdaySet::from_json(&json!(["mon", "xyz"])).unwrap_err();
        assert!(err
            .to_string()
            .starts_with(r#"$[1]: invalid weekday set "xyz""#));
    }

    #[test]
    fn calendars_combine_exclusions_and_windows() {
        let mut cal = CalendarSpec::new("ops");
        assert!(
            cal.allows(date("2025-03-08"), time("03:00")),
            "empty calendar allows all"
        );
        cal.exclude_weekdays = "sun".parse().unwrap();
        cal.exclude_dates.insert(date("2025-03-10"));
        cal.windows.push(TimeWindow::new(
            WeekdaySet::EMPTY,
            time("08:00"),
            time("12:00"),
        ));
        assert!(cal.allows(date("2025-03-08"), time("08:00")));
        assert!(
            !cal.allows(date("2025-03-09"), time("09:00")),
            "sunday excluded"
        );
        assert!(
            !cal.allows(date("2025-03-10"), time("09:00")),
            "date excluded"
        );
        assert!(
            !cal.allows(date("2025-03-11"), time("12:00")),
            "outside window"
        );
        let back = CalendarSpec::from_json(&cal.to_json()).unwrap();
        assert_eq!(back, cal);
        assert_eq!(
            cal.to_json(),
            json!({
                "name": "ops", "timezone": "UTC", "exclude_dates": ["2025-03-10"],
                "exclude_weekdays": ["sun"], "windows": [{"start": "08:00", "end": "12:00"}]
            })
        );
    }

    #[test]
    fn calendar_validation_and_decode_errors() {
        let mut cal = CalendarSpec::new("Ops Team");
        cal.exclude_weekdays = WeekdaySet::ALL;
        cal.windows.push(TimeWindow::new(
            WeekdaySet::EMPTY,
            time("24:00"),
            time("24:00"),
        ));
        let lines: Vec<String> = cal
            .validate()
            .unwrap_err()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            lines,
            [
                "$.name: must start with a lowercase letter or digit, got 'O'",
                "$.exclude_weekdays: excludes every day, so nothing could ever fire",
                "$.windows[0].start: 24:00 is only valid as an end time",
                "$.windows[0].end: must differ from start; use 00:00-24:00 for the whole day",
            ]
        );
        let err = CalendarSpec::from_json(&json!({
            "name": "x", "exclude_dates": ["2025-01-01", "2025-02-30"]
        }))
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            r#"$.exclude_dates[1]: invalid date "2025-02-30": day must be 01-28 in that month"#
        );
        let err = CalendarSpec::from_json(&json!({
            "name": "x", "windows": [{"start": "9am", "end": "17:00"}]
        }))
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            r#"$.windows[0].start: invalid time of day "9am": expected HH:MM"#
        );
    }
}
