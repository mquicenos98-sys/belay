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
