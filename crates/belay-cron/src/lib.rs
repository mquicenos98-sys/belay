//! Cron expressions with time zones and daylight-saving-correct iteration.
//!
//! This crate parses cron expressions, evaluates them in a time zone and
//! finds fire times efficiently. It has no dependencies: calendar math, the
//! POSIX TZ rule engine and a table of common IANA zones are built in.
//! Instants are Unix seconds (`i64`) throughout.
//!
//! ```
//! use belay_cron::{CronExpr, Tz};
//!
//! let expr = CronExpr::parse("30 9 * * MON-FRI")?;
//! let tz = Tz::named("America/New_York")?;
//! assert_eq!(expr.describe(), "at 09:30 on Monday through Friday");
//!
//! // Friday 2025-03-07T15:00:00Z. The US switches to daylight time on the
//! // Sunday, so Monday's 09:30 is at 13:30 UTC instead of 14:30.
//! let next = expr.next_after(1_741_359_600, &tz).unwrap();
//! assert_eq!(tz.format_rfc3339(next), "2025-03-10T09:30:00-04:00");
//! # Ok::<(), belay_cron::CronError>(())
//! ```
//!
//! # Expressions
//!
//! See [`CronExpr::parse`] for the syntax. Five fields are standard; a
//! leading seconds field and a trailing year field are optional. When both
//! day fields are restricted a day matches if *either* does (Vixie cron);
//! [`DayMatch::Both`] selects the stricter reading, and [`Dialect::Quartz`]
//! accepts Quartz's numbering. `Display` prints a normalised expression and
//! [`CronExpr::describe`] an English one.
//!
//! # Time zones
//!
//! [`Tz`] is UTC, a fixed offset, or a POSIX TZ rule such as
//! `CET-1CEST,M3.5.0,M10.5.0/3`. [`Tz::named`] maps IANA names from the
//! embedded table ([`zone_names`]) to their current rule. Only the current
//! rule is modelled: historical offsets and past rule changes are not, so
//! instants from years before a zone's last change use today's rule.
//!
//! # Daylight saving time
//!
//! When clocks jump forward, a matching wall-clock time inside the gap fires
//! once, at the first instant after the gap. When clocks go back, a matching
//! time in the repeated hour fires once, on its first occurrence, unless the
//! expression is periodic (it fires several times an hour or every hour),
//! in which case it fires on both passes as cronie does for its wildcard
//! jobs. [`CronExpr::next_after`] has the details.
//!
//! # Range
//!
//! Iteration covers wall-clock times from 1970-01-01 to the end of 2199,
//! the same range the year field accepts. Searches past that end return
//! `None`, as do expressions that can never match (`0 0 30 2 *`), which are
//! recognised without scanning.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod civil;
mod describe;
mod error;
mod expr;
mod format;
mod parse;
mod search;
mod tz;

pub use civil::{LocalDateTime, Weekday};
pub use error::{CronError, Field, FieldProblem, Result};
pub use expr::CronExpr;
pub use parse::{DayMatch, Dialect, ParseOptions};
pub use search::{Preceding, Upcoming};
pub use tz::zones::zone_names;
pub use tz::{LocalResult, PosixTz, Transition, Tz};

/// First year the year field accepts and iteration reaches.
pub const MIN_YEAR: u32 = 1970;
/// Last year the year field accepts and iteration reaches.
pub const MAX_YEAR: u32 = 2199;
