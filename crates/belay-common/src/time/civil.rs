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
