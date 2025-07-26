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
