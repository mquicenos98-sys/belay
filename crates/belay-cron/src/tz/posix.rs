//! POSIX TZ rule strings: parsing, evaluation and normalised printing.
//!
//! Grammar (POSIX.1-2017 §8.3, with the RFC 8536 §3.3.1 extensions that
//! allow signed transition times up to ±167 hours):
//!
//! ```text
//! std offset [dst [offset] [,start[/time],end[/time]]]
//! ```
//!
//! Offsets are written as *time to add to local time to reach UTC*, so
//! `EST5` is five hours **west** of Greenwich; internally every offset is
//! stored the other way round (seconds east, as in `+05:30`). Transition
//! dates are `Jn` (1-365, February 29 never counted), `n` (0-365, counting
//! February 29) or `Mm.w.d` (weekday `d` of week `w` of month `m`, `w = 5`
//! meaning the last one). A start time is read on the standard-time clock and
//! an end time on the daylight-time clock; both default to 02:00.

use std::fmt;

use crate::civil::{
    civil_from_days, days_from_civil, days_in_month, is_leap_year, weekday_from_days, SECS_PER_DAY,
};
use crate::error::{CronError, Result};

/// Largest accepted UTC offset magnitude: 24:59:59, the POSIX limit.
pub(crate) const MAX_OFFSET: i32 = 24 * 3600 + 59 * 60 + 59;
const DEFAULT_RULE_TIME: i32 = 2 * 3600;

/// A parsed POSIX TZ rule such as `CET-1CEST,M3.5.0,M10.5.0/3`.
///
/// Rules loaded from the embedded zone table also remember the IANA name
/// they came from (see [`name`](Self::name)); `Display` prints the
/// normalised rule string either way.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PosixTz {
    std_abbr: String,
    std_offset: i32,
    dst: Option<DstRule>,
    name: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DstRule {
    abbr: String,
    offset: i32,
    start: RuleTime,
    end: RuleTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct RuleTime {
    date: RuleDate,
    /// Seconds after local midnight of `date`; may be negative or exceed a day.
    time: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum RuleDate {
    /// `Jn`: day 1-365 of the year, February 29 is never counted.
    Julian(u16),
    /// `n`: zero-based day 0-365 of the year, February 29 included.
    ZeroBased(u16),
    /// `Mm.w.d`.
    MonthWeekDay { month: u8, week: u8, weekday: u8 },
}

impl RuleDate {
    /// Day number (days since 1970-01-01) the rule selects in `year`.
    fn day_number(self, year: i64) -> i64 {
        match self {
            RuleDate::Julian(n) => {
                let skip_leap_day = is_leap_year(year) && n >= 60;
                days_from_civil(year, 1, 1) + i64::from(n) - 1 + i64::from(skip_leap_day)
            }
            RuleDate::ZeroBased(n) => days_from_civil(year, 1, 1) + i64::from(n),
            RuleDate::MonthWeekDay {
                month,
                week,
                weekday,
            } => {
                let month = u32::from(month);
                let first = days_from_civil(year, month, 1);
                let first_wd = weekday_from_days(first);
                let offset = (u32::from(weekday) + 7 - first_wd) % 7;
                let mut day = offset + 7 * (u32::from(week) - 1);
                // Week 5 means "the last one", which may be the fourth.
                while day > days_in_month(year, month) {
                    day -= 7;
                }
                first + i64::from(day)
            }
        }
    }
}

impl RuleTime {
    /// Naive local seconds of this transition in `year`.
    fn local_secs(self, year: i64) -> i64 {
        self.date.day_number(year) * SECS_PER_DAY + i64::from(self.time)
    }
}

impl DstRule {
    /// UTC instants of the DST start and end transitions in `year`.
    fn transitions(&self, year: i64, std_offset: i32) -> (i64, i64) {
        let start = self.start.local_secs(year) - i64::from(std_offset);
        let end = self.end.local_secs(year) - i64::from(self.offset);
        (start, end)
    }

    fn active_at(&self, utc: i64, std_offset: i32) -> bool {
        // The year is taken from the standard-time reading. That keeps a
        // southern-hemisphere summer, which spans New Year, consistent: both
        // halves consult the rule for the year in which DST started or ends.
        let year = year_of(utc + i64::from(std_offset));
        active_between(self.transitions(year, std_offset), utc)
    }
}

/// Whether `utc` falls in daylight time given one year's `(start, end)`.
/// `start > end` is a southern-hemisphere rule: DST spans New Year.
fn active_between((start, end): (i64, i64), utc: i64) -> bool {
    match start.cmp(&end) {
        std::cmp::Ordering::Less => start <= utc && utc < end,
        std::cmp::Ordering::Greater => !(end <= utc && utc < start),
        std::cmp::Ordering::Equal => false,
    }
}

/// Number of consecutive years whose transitions [`PosixTz::neighbours`]
/// examines, centred on the year of the query. A year either side is enough
/// for well-formed rules; the extra year guards against rules whose extended
/// transition times (RFC 8536 allows ±167 hours) spill into the next year.
const WINDOW_YEARS: usize = 5;

fn year_of(naive: i64) -> i64 {
    civil_from_days(naive.div_euclid(SECS_PER_DAY)).0
}

impl PosixTz {
    /// Parses a POSIX TZ string.
    ///
    /// ```
    /// use belay_cron::PosixTz;
    ///
    /// let tz = PosixTz::parse("EST5EDT,M3.2.0,M11.1.0")?;
    /// assert_eq!(tz.std_offset(), -5 * 3600);
    /// assert_eq!(tz.dst_offset(), Some(-4 * 3600));
    /// assert_eq!(PosixTz::parse("<+0330>-3:30")?.std_offset(), 12_600);
    /// # Ok::<(), belay_cron::CronError>(())
    /// ```
    pub fn parse(input: &str) -> Result<PosixTz> {
        Parser::new(input).parse()
    }

    pub(crate) fn with_name(mut self, name: &'static str) -> PosixTz {
        self.name = Some(name);
        self
    }

    /// The IANA zone name, for rules that came from the embedded table.
    #[must_use]
    pub fn name(&self) -> Option<&'static str> {
        self.name
    }

    /// Abbreviation of standard time, e.g. `"EST"` or `"+0330"`.
    #[must_use]
    pub fn std_abbreviation(&self) -> &str {
        &self.std_abbr
    }

    /// Standard-time offset in seconds east of UTC.
    #[must_use]
    pub fn std_offset(&self) -> i32 {
        self.std_offset
    }

    /// Abbreviation of daylight saving time, if the rule has one.
    #[must_use]
    pub fn dst_abbreviation(&self) -> Option<&str> {
        self.dst.as_ref().map(|d| d.abbr.as_str())
    }

    /// Daylight-saving offset in seconds east of UTC, if the rule has one.
    #[must_use]
    pub fn dst_offset(&self) -> Option<i32> {
        self.dst.as_ref().map(|d| d.offset)
    }

    /// Whether the rule switches between two offsets during the year.
    #[must_use]
    pub fn has_transitions(&self) -> bool {
        self.dst
            .as_ref()
            .is_some_and(|d| d.offset != self.std_offset)
    }

    /// Whether the rule's daylight-saving half is in force at `utc`.
    ///
    /// This is the rule's own flag: with a negative-DST rule such as
    /// Europe/Dublin's `IST-1GMT0,…` it is set in winter.
    #[must_use]
    pub fn is_dst_at(&self, utc: i64) -> bool {
        self.dst
            .as_ref()
            .is_some_and(|d| d.active_at(utc, self.std_offset))
    }

    /// UTC offset in seconds east at the instant `utc`.
    #[must_use]
    pub fn offset_at(&self, utc: i64) -> i32 {
        match &self.dst {
            Some(d) if d.active_at(utc, self.std_offset) => d.offset,
            _ => self.std_offset,
        }
    }

    /// Abbreviation in force at `utc`.
    #[must_use]
    pub fn abbreviation_at(&self, utc: i64) -> &str {
        match &self.dst {
            Some(d) if d.active_at(utc, self.std_offset) => &d.abbr,
            _ => &self.std_abbr,
        }
    }

    /// The last real transition at or before `utc` and the first one after
    /// it.
    ///
    /// Candidates are each year's DST start and end, but not every candidate
    /// changes the offset: `EST5EDT,0/0,J365/25` (daylight time all year)
    /// ends one year at the instant it starts the next. A candidate counts
    /// only if the offset really differs on either side of it, judged by the
    /// same year-based rule as [`offset_at`](Self::offset_at).
    ///
    /// TODO(perf): iteration in zones with DST spends most of its time here
    /// (about 0.5 µs a call); memoising the last window's transitions would
    /// make consecutive `next_after` calls in the same year nearly free.
    pub(crate) fn neighbours(&self, utc: i64) -> (Option<i64>, Option<i64>) {
        let Some(dst) = self.dst.as_ref().filter(|d| d.offset != self.std_offset) else {
            return (None, None);
        };
        let std = self.std_offset;
        let first_year = year_of(utc + i64::from(std)) - (WINDOW_YEARS / 2) as i64;
        let mut pairs = [(0i64, 0i64); WINDOW_YEARS];
        for (i, pair) in pairs.iter_mut().enumerate() {
            *pair = dst.transitions(first_year + i as i64, std);
        }
        let offset = |t: i64| {
            let y = year_of(t + i64::from(std));
            let pair = match usize::try_from(y - first_year) {
                Ok(i) if i < WINDOW_YEARS => pairs[i],
                _ => dst.transitions(y, std),
            };
            if active_between(pair, t) {
                dst.offset
            } else {
                std
            }
        };
        let changes = |c: i64| offset(c - 1) != offset(c);
        let mut candidates = [0i64; 2 * WINDOW_YEARS];
        for (i, (start, end)) in pairs.iter().enumerate() {
            candidates[2 * i] = *start;
            candidates[2 * i + 1] = *end;
        }
        candidates.sort_unstable();
        let prev = candidates
            .iter()
            .rev()
            .copied()
            .filter(|&c| c <= utc)
            .find(|&c| changes(c));
        let next = candidates
            .iter()
            .copied()
            .filter(|&c| c > utc)
            .find(|&c| changes(c));
        (prev, next)
    }

    /// First instant strictly after `utc` at which the offset changes.
    pub(crate) fn next_transition_at(&self, utc: i64) -> Option<i64> {
        self.neighbours(utc).1
    }

    /// Last instant at or before `utc` at which the offset changed.
    pub(crate) fn prev_transition_at(&self, utc: i64) -> Option<i64> {
        self.neighbours(utc).0
    }
}

impl fmt::Display for PosixTz {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_abbr(f, &self.std_abbr)?;
        write_hms(f, -self.std_offset)?;
        if let Some(dst) = &self.dst {
            write_abbr(f, &dst.abbr)?;
            if dst.offset != self.std_offset + 3600 {
                write_hms(f, -dst.offset)?;
            }
            for rule in [dst.start, dst.end] {
                f.write_str(",")?;
                match rule.date {
                    RuleDate::Julian(n) => write!(f, "J{n}")?,
                    RuleDate::ZeroBased(n) => write!(f, "{n}")?,
                    RuleDate::MonthWeekDay {
                        month,
                        week,
                        weekday,
                    } => write!(f, "M{month}.{week}.{weekday}")?,
                }
                if rule.time != DEFAULT_RULE_TIME {
                    f.write_str("/")?;
                    write_hms(f, rule.time)?;
                }
            }
        }
        Ok(())
    }
}

fn write_abbr(f: &mut fmt::Formatter<'_>, abbr: &str) -> fmt::Result {
    if abbr.bytes().all(|b| b.is_ascii_alphabetic()) {
        f.write_str(abbr)
    } else {
        write!(f, "<{abbr}>")
    }
}

/// Writes `[-]h[:mm[:ss]]`, the shortest POSIX spelling.
fn write_hms(f: &mut fmt::Formatter<'_>, secs: i32) -> fmt::Result {
    if secs < 0 {
        f.write_str("-")?;
    }
    let secs = secs.unsigned_abs();
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    write!(f, "{h}")?;
    if m != 0 || s != 0 {
        write!(f, ":{m:02}")?;
    }
    if s != 0 {
        write!(f, ":{s:02}")?;
    }
    Ok(())
}

/// Recursive-descent parser over the bytes of a TZ string.
struct Parser<'a> {
    input: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Parser {
            input,
            bytes: input.as_bytes(),
            pos: 0,
        }
    }

    fn error(&self, reason: &'static str) -> CronError {
        self.error_at(self.pos, reason)
    }

    fn error_at(&self, pos: usize, reason: &'static str) -> CronError {
        let column = self.input[..pos.min(self.input.len())].chars().count() + 1;
        CronError::InvalidTz {
            input: self.input.to_string(),
            column,
            reason,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn eat(&mut self, b: u8) -> bool {
        if self.peek() == Some(b) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn parse(mut self) -> Result<PosixTz> {
        if self.bytes.is_empty() {
            return Err(self.error("empty rule"));
        }
        let std_abbr = self.abbreviation()?;
        if self.peek().is_none() {
            return Err(self.error("expected a UTC offset after the abbreviation"));
        }
        let std_offset = -self.hms(24, true)?;
        if self.peek().is_none() {
            return Ok(PosixTz {
                std_abbr,
                std_offset,
                dst: None,
                name: None,
            });
        }
        let dst_abbr = self.abbreviation()?;
        let dst_offset = match self.peek() {
            Some(b) if b == b'+' || b == b'-' || b.is_ascii_digit() => -self.hms(24, true)?,
            _ => std_offset + 3600,
        };
        if self.peek().is_none() {
            return Err(
                self.error("daylight saving time needs start and end rules, as in ,M3.2.0,M11.1.0")
            );
        }
        if !self.eat(b',') {
            return Err(self.error("expected ',' before the daylight saving start rule"));
        }
        let start = self.rule_time()?;
        if !self.eat(b',') {
            return Err(self.error("expected ',' before the daylight saving end rule"));
        }
        let end = self.rule_time()?;
        if self.peek().is_some() {
            return Err(self.error("unexpected text after the end rule"));
        }
        Ok(PosixTz {
            std_abbr,
            std_offset,
            dst: Some(DstRule {
                abbr: dst_abbr,
                offset: dst_offset,
                start,
                end,
            }),
            name: None,
        })
    }

    fn abbreviation(&mut self) -> Result<String> {
        let start = self.pos;
        if self.eat(b'<') {
            let inner_start = self.pos;
            while let Some(b) = self.peek() {
                if b.is_ascii_alphanumeric() || b == b'+' || b == b'-' {
                    self.pos += 1;
                } else {
                    break;
                }
            }
            let inner = &self.input[inner_start..self.pos];
            if !self.eat(b'>') {
                return Err(self.error("expected '>' to close the quoted abbreviation"));
            }
            if inner.len() < 3 {
                self.pos = start;
                return Err(self.error("abbreviations need at least three characters"));
            }
            return Ok(inner.to_string());
        }
        while self.peek().is_some_and(|b| b.is_ascii_alphabetic()) {
            self.pos += 1;
        }
        if self.pos - start < 3 {
            self.pos = start;
            return Err(
                self.error("expected an abbreviation of at least three letters (or <...> quoted)")
            );
        }
        Ok(self.input[start..self.pos].to_string())
    }

    /// `[+-]hh[:mm[:ss]]` in seconds, with the hour bounded by `max_hours`.
    fn hms(&mut self, max_hours: u32, allow_sign: bool) -> Result<i32> {
        let negative = if allow_sign && self.eat(b'-') {
            true
        } else {
            if allow_sign {
                self.eat(b'+');
            }
            false
        };
        let at = self.pos;
        let hours = self.number(3).ok_or_else(|| self.error("expected hours"))?;
        if hours > max_hours {
            return Err(self.error_at(at, "hours out of range"));
        }
        let mut secs = hours * 3600;
        for unit in [60, 1] {
            if !self.eat(b':') {
                break;
            }
            let at = self.pos;
            let v = self
                .number(2)
                .filter(|_| self.pos - at == 2)
                .ok_or_else(|| self.error_at(at, "expected two-digit minutes or seconds"))?;
            if v > 59 {
                return Err(self.error_at(at, "minutes and seconds must be 0-59"));
            }
            secs += v * unit;
        }
        let secs = secs as i32;
        Ok(if negative { -secs } else { secs })
    }

    fn number(&mut self, max_digits: usize) -> Option<u32> {
        let start = self.pos;
        while self.pos - start < max_digits && self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.pos == start {
            return None;
        }
        self.input[start..self.pos].parse().ok()
    }

    fn rule_time(&mut self) -> Result<RuleTime> {
        let date = self.rule_date()?;
        let time = if self.eat(b'/') {
            self.hms(167, true)?
        } else {
            DEFAULT_RULE_TIME
        };
        Ok(RuleTime { date, time })
    }

    fn rule_date(&mut self) -> Result<RuleDate> {
        if self.eat(b'J') {
            let at = self.pos;
            let n = self
                .number(3)
                .ok_or_else(|| self.error("expected a day number after 'J'"))?;
            if !(1..=365).contains(&n) {
                return Err(self.error_at(at, "Julian day must be 1-365"));
            }
            return Ok(RuleDate::Julian(n as u16));
        }
        if self.eat(b'M') {
            let at = self.pos;
            let month = self
                .number(2)
                .ok_or_else(|| self.error("expected a month"))?;
            if !(1..=12).contains(&month) {
                return Err(self.error_at(at, "month must be 1-12"));
            }
            if !self.eat(b'.') {
                return Err(self.error("expected '.' after the month"));
            }
            let at = self.pos;
            let week = self
                .number(1)
                .ok_or_else(|| self.error("expected a week"))?;
            if !(1..=5).contains(&week) {
                return Err(self.error_at(at, "week must be 1-5"));
            }
            if !self.eat(b'.') {
                return Err(self.error("expected '.' after the week"));
            }
            let at = self.pos;
            let weekday = self
                .number(1)
                .ok_or_else(|| self.error("expected a weekday"))?;
            if weekday > 6 {
                return Err(self.error_at(at, "weekday must be 0-6"));
            }
            return Ok(RuleDate::MonthWeekDay {
                month: month as u8,
                week: week as u8,
                weekday: weekday as u8,
            });
        }
        let at = self.pos;
        match self.number(3) {
            Some(n) if n <= 365 => Ok(RuleDate::ZeroBased(n as u16)),
            Some(_) => Err(self.error_at(at, "zero-based day must be 0-365")),
            None => Err(self.error("expected a transition date (Jn, n or Mm.w.d)")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(y: i64, mo: u32, d: u32, h: i64, mi: i64) -> i64 {
        days_from_civil(y, mo, d) * SECS_PER_DAY + h * 3600 + mi * 60
    }

    #[test]
    fn parses_us_eastern_and_computes_2025_transitions() {
        let tz = PosixTz::parse("EST5EDT,M3.2.0,M11.1.0").unwrap();
        assert_eq!(tz.std_abbreviation(), "EST");
        assert_eq!(tz.dst_abbreviation(), Some("EDT"));
        // 2025-03-09 02:00 EST == 07:00 UTC; 2025-11-02 02:00 EDT == 06:00 UTC.
        let start = ts(2025, 3, 9, 7, 0);
        let end = ts(2025, 11, 2, 6, 0);
        assert_eq!(tz.offset_at(start - 1), -5 * 3600);
        assert_eq!(tz.offset_at(start), -4 * 3600);
        assert_eq!(tz.offset_at(end - 1), -4 * 3600);
        assert_eq!(tz.offset_at(end), -5 * 3600);
        assert_eq!(tz.next_transition_at(start - 1), Some(start));
        assert_eq!(tz.next_transition_at(start), Some(end));
        assert_eq!(tz.prev_transition_at(start), Some(start));
        assert_eq!(
            tz.prev_transition_at(start - 1),
            Some(ts(2024, 11, 3, 6, 0))
        );
        assert_eq!(tz.abbreviation_at(start), "EDT");
    }

    #[test]
    fn southern_hemisphere_rule_is_dst_across_new_year() {
        let tz = PosixTz::parse("AEST-10AEDT,M10.1.0,M4.1.0/3").unwrap();
        assert_eq!(tz.offset_at(ts(2025, 1, 1, 0, 0)), 11 * 3600);
        assert_eq!(tz.offset_at(ts(2024, 12, 31, 13, 30)), 11 * 3600);
        assert_eq!(tz.offset_at(ts(2025, 7, 1, 0, 0)), 10 * 3600);
        // Ends 2025-04-06 03:00 AEDT (16:00 UTC the day before), starts
        // 2025-10-05 02:00 AEST (16:00 UTC the day before).
        let end = ts(2025, 4, 5, 16, 0);
        let start = ts(2025, 10, 4, 16, 0);
        assert_eq!(tz.next_transition_at(ts(2025, 1, 1, 0, 0)), Some(end));
        assert_eq!(tz.next_transition_at(end), Some(start));
        assert!(tz.is_dst_at(start));
        assert!(!tz.is_dst_at(end));
    }

    #[test]
    fn negative_dst_rule_uses_the_lower_offset_in_winter() {
        let tz = PosixTz::parse("IST-1GMT0,M10.5.0,M3.5.0/1").unwrap();
        assert_eq!(tz.offset_at(ts(2025, 1, 15, 12, 0)), 0);
        assert!(tz.is_dst_at(ts(2025, 1, 15, 12, 0)));
        assert_eq!(tz.offset_at(ts(2025, 7, 15, 12, 0)), 3600);
        assert_eq!(tz.abbreviation_at(ts(2025, 7, 15, 12, 0)), "IST");
        // Same instants as London: 2025-03-30 and 2025-10-26 at 01:00 UTC.
        assert_eq!(
            tz.next_transition_at(ts(2025, 1, 1, 0, 0)),
            Some(ts(2025, 3, 30, 1, 0))
        );
        assert_eq!(
            tz.next_transition_at(ts(2025, 3, 30, 1, 0)),
            Some(ts(2025, 10, 26, 1, 0))
        );
    }

    #[test]
    fn transition_times_may_be_negative_or_exceed_a_day() {
        // America/Nuuk: -01:00 local on the Saturday before the last Sunday.
        let nuuk = PosixTz::parse("<-02>2<-01>,M3.5.0/-1,M10.5.0/0").unwrap();
        assert_eq!(
            nuuk.next_transition_at(ts(2025, 1, 1, 0, 0)),
            Some(ts(2025, 3, 30, 1, 0))
        );
        // Asia/Jerusalem: 26:00 on the fourth Thursday is Friday 02:00.
        let jlm = PosixTz::parse("IST-2IDT,M3.4.4/26,M10.5.0").unwrap();
        assert_eq!(
            jlm.next_transition_at(ts(2025, 1, 1, 0, 0)),
            Some(ts(2025, 3, 28, 0, 0))
        );
        // America/Santiago: Saturday 24:00.
        let scl = PosixTz::parse("<-04>4<-03>,M9.1.6/24,M4.1.6/24").unwrap();
        assert_eq!(
            scl.next_transition_at(ts(2025, 1, 1, 0, 0)),
            Some(ts(2025, 4, 6, 3, 0))
        );
        assert_eq!(
            scl.next_transition_at(ts(2025, 4, 6, 3, 0)),
            Some(ts(2025, 9, 7, 4, 0))
        );
    }

    #[test]
    fn julian_forms_count_leap_days_differently() {
        let j = RuleDate::Julian(60);
        let n = RuleDate::ZeroBased(59);
        // J60 is always March 1; zero-based 59 is Feb 29 in a leap year.
        assert_eq!(j.day_number(2024), days_from_civil(2024, 3, 1));
        assert_eq!(j.day_number(2025), days_from_civil(2025, 3, 1));
        assert_eq!(n.day_number(2024), days_from_civil(2024, 2, 29));
        assert_eq!(n.day_number(2025), days_from_civil(2025, 3, 1));
        assert_eq!(
            RuleDate::Julian(365).day_number(2024),
            days_from_civil(2024, 12, 31)
        );
    }

    #[test]
    fn permanent_dst_rule_never_transitions() {
        // RFC 8536 §3.3.1's example of daylight saving all year round.
        let tz = PosixTz::parse("EST5EDT,0/0,J365/25").unwrap();
        for t in [
            ts(2025, 1, 1, 0, 0),
            ts(2025, 1, 1, 4, 59),
            ts(2025, 6, 1, 0, 0),
            ts(2025, 12, 31, 23, 0),
            ts(2026, 1, 1, 5, 0),
        ] {
            assert_eq!(tz.offset_at(t), -4 * 3600, "at {t}");
        }
        assert_eq!(tz.next_transition_at(ts(2025, 6, 1, 0, 0)), None);
        assert_eq!(tz.prev_transition_at(ts(2025, 6, 1, 0, 0)), None);
    }

    #[test]
    fn display_normalises_and_round_trips() {
        for (input, normal) in [
            ("EST5EDT,M3.2.0,M11.1.0", "EST5EDT,M3.2.0,M11.1.0"),
            ("EST+5EDT4,M3.2.0/2,M11.1.0/2:00", "EST5EDT,M3.2.0,M11.1.0"),
            ("<+0330>-3:30", "<+0330>-3:30"),
            ("<-03>3", "<-03>3"),
            ("IST-5:30", "IST-5:30"),
            (
                "<+1245>-12:45<+1345>,M9.5.0/2:45,M4.1.0/3:45",
                "<+1245>-12:45<+1345>,M9.5.0/2:45,M4.1.0/3:45",
            ),
            (
                "<-02>2<-01>,M3.5.0/-1,M10.5.0/0",
                "<-02>2<-01>,M3.5.0/-1,M10.5.0/0",
            ),
            ("AAA3BBB,J60/1:30:15,300/-2", "AAA3BBB,J60/1:30:15,300/-2"),
            ("XYZ0ABC-2,M3.5.0,M10.5.0", "XYZ0ABC-2,M3.5.0,M10.5.0"),
        ] {
            let tz = PosixTz::parse(input).unwrap();
            assert_eq!(tz.to_string(), normal, "{input}");
            assert_eq!(PosixTz::parse(normal).unwrap(), tz, "{input}");
        }
    }

    #[test]
    fn rejects_malformed_rules_with_column() {
        let cases = [
            ("", 1, "empty rule"),
            (
                "ES5",
                1,
                "expected an abbreviation of at least three letters (or <...> quoted)",
            ),
            ("EST", 4, "expected a UTC offset after the abbreviation"),
            ("EST25", 4, "hours out of range"),
            (
                "EST5EDT",
                8,
                "daylight saving time needs start and end rules, as in ,M3.2.0,M11.1.0",
            ),
            ("EST5EDT,M3.2", 13, "expected '.' after the week"),
            ("EST5EDT,M13.2.0,M11.1.0", 10, "month must be 1-12"),
            ("EST5EDT,M3.6.0,M11.1.0", 12, "week must be 1-5"),
            ("EST5EDT,M3.2.7,M11.1.0", 14, "weekday must be 0-6"),
            ("EST5EDT,J0,M11.1.0", 10, "Julian day must be 1-365"),
            ("EST5EDT,366,M11.1.0", 9, "zero-based day must be 0-365"),
            ("EST5EDT,M3.2.0/168,M11.1.0", 16, "hours out of range"),
            (
                "EST5EDT,M3.2.0",
                15,
                "expected ',' before the daylight saving end rule",
            ),
            ("<+05-5", 7, "expected '>' to close the quoted abbreviation"),
            ("<+5>-5", 1, "abbreviations need at least three characters"),
            ("EST5:7", 6, "expected two-digit minutes or seconds"),
            ("EST5:75", 6, "minutes and seconds must be 0-59"),
            (
                "EST5EDT,M3.2.0,M11.1.0x",
                23,
                "unexpected text after the end rule",
            ),
        ];
        for (input, column, reason) in cases {
            match PosixTz::parse(input) {
                Err(CronError::InvalidTz {
                    column: c,
                    reason: r,
                    ..
                }) => {
                    assert_eq!((c, r), (column, reason), "{input:?}");
                }
                other => panic!("{input:?}: expected InvalidTz, got {other:?}"),
            }
        }
    }

    #[test]
    fn fixed_rule_has_no_transitions() {
        let tz = PosixTz::parse("<+0545>-5:45").unwrap();
        assert!(!tz.has_transitions());
        assert_eq!(tz.offset_at(0), 5 * 3600 + 45 * 60);
        assert_eq!(tz.next_transition_at(0), None);
        assert!(!tz.is_dst_at(0));
    }
}
