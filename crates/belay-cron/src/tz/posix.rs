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
