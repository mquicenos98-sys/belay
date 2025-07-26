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
