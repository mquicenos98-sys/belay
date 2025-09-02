//! When a schedule fires: its cron expression evaluated in its time zone,
//! clipped to its `[starts_at, ends_at)` bounds and filtered by its
//! calendar.
//!
//! belay-cron works in Unix seconds; everything here speaks
//! [`Timestamp`]s (milliseconds) and converts at the edge. Fire times are
//! always whole seconds.
//!
//! # Calendar vetoes
//!
//! A cron match that the calendar does not allow is *vetoed*: it is not a
//! fire time. The search walks past vetoed matches one by one, counting
//! them, so that the scheduler can report a closed stretch as a single
//! skip. After [`COUNT_LIMIT`] vetoes in a row it stops counting and jumps
//! straight to the next instant the calendar opens, so an every-second
//! expression under a weekday calendar costs no more than an hourly one.

use std::sync::Arc;
use std::time::Duration;

use belay_common::hash::hash64;
use belay_common::Timestamp;
use belay_cron::{CronExpr, Tz};
use belay_proto::limits::{MAX_CRON_LEN, MAX_TIMEZONE_LEN};
use belay_proto::{ScheduleId, ScheduleSpec};

use super::calendar::CompiledCalendar;

/// Counts of skipped or missed fire times stop at this value.
pub(crate) const COUNT_LIMIT: u32 = 10_000;

/// Calendar jumps one search may make before concluding that the cron
/// expression and the calendar never meet again.
const MAX_JUMPS: u32 = 2_000;

/// Seed for jitter offsets; changing it would move every jittered job.
const JITTER_SEED: u64 = 0x6265_6c61_795f_6a74;

/// Parses a cron expression, with messages fit for a `cron` violation.
pub(crate) fn parse_cron(text: &str) -> Result<CronExpr, String> {
    if text.len() > MAX_CRON_LEN {
        return Err(format!(
            "must be at most {MAX_CRON_LEN} bytes, got {}",
            text.len()
        ));
    }
    let expr = CronExpr::parse(text).map_err(|e| e.to_string())?;
    if !expr.is_satisfiable() {
        return Err(format!(
            "{text:?} can never match (no date satisfies its day and month fields)"
        ));
    }
    Ok(expr)
}

/// Parses a time zone (IANA name, fixed offset or POSIX rule), with
/// messages fit for a `timezone` violation.
pub(crate) fn parse_timezone(text: &str) -> Result<Tz, String> {
    if text.is_empty() {
        return Err("must not be empty".to_owned());
    }
    if text.len() > MAX_TIMEZONE_LEN {
        return Err(format!(
            "must be at most {MAX_TIMEZONE_LEN} bytes, got {}",
            text.len()
        ));
    }
    if text.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("must not contain spaces or control characters".to_owned());
    }
    Tz::parse(text).map_err(|e| e.to_string())
}
