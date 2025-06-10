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
