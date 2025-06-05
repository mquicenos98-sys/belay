//! The crate's error type.
//!
//! One enum covers expression, time zone and date-time parsing so callers
//! validating a schedule (`cron` + `timezone`) handle a single type. Every
//! message is lowercase and names the offending field or input; errors that
//! come from a specific character also carry its 1-based column so a CLI can
//! draw a caret under it.

use std::fmt;

/// Result alias used throughout the crate.
pub type Result<T, E = CronError> = std::result::Result<T, E>;

/// One of the (up to) seven fields of a cron expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field {
    /// Seconds, 0-59 (6- and 7-field forms only).
    Second,
    /// Minutes, 0-59.
    Minute,
    /// Hours, 0-23.
    Hour,
    /// Day of the month, 1-31.
    DayOfMonth,
    /// Month, 1-12 or `JAN`-`DEC`.
    Month,
    /// Day of the week, 0-7 (0 and 7 are Sunday) or `SUN`-`SAT`.
    DayOfWeek,
    /// Year, 1970-2199 (7-field form only).
    Year,
}
