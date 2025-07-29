//! The parsed expression and its matching rules.
//!
//! Every field is a bit set in the canonical numbering (weekdays 0-6 from
//! Sunday, months 1-12, days 1-31, years as offsets from 1970). The day
//! fields also carry their special items (`L`, `W`, `#`), which only make
//! sense relative to a concrete month, so day matching works a month at a
//! time: [`CronExpr::day_mask`] gives the set of matching days of one month.
//!
//! Whether a day can match at all depends only on the month's length and the
//! weekday it starts on. A year therefore has one of 14 "types" (leap or not,
//! times the weekday of January 1), and for each type the expression records
//! which months contain a matching day. The search uses that to skip whole
//! years and months, and to recognise expressions that can never fire
//! (`0 0 30 2 *`) without scanning anything.

use std::fmt;
use std::str::FromStr;

use crate::civil::{
    days_from_civil, days_in_month, is_leap_year, weekday_from_days, LocalDateTime,
};
use crate::error::{CronError, Result};
use crate::parse::ParseOptions;
use crate::{MAX_YEAR, MIN_YEAR};

pub(crate) const ALL_SECONDS: u64 = (1 << 60) - 1;

pub(crate) const ALL_MINUTES: u64 = ALL_SECONDS;

pub(crate) const ALL_HOURS: u32 = (1 << 24) - 1;

/// Bits 1-31.
pub(crate) const ALL_DAYS: u32 = !1;

/// Bits 1-12.
pub(crate) const ALL_MONTHS: u16 = 0x1FFE;

pub(crate) const ALL_WEEKDAYS: u8 = 0x7F;

/// A parsed cron expression.
///
/// Build one with [`CronExpr::parse`] or [`CronExpr::parse_with`]. Two
/// expressions are equal when they select the same times under the same
/// options, whatever their spelling: `*/15` and `0,15,30,45` compare equal.
/// `Display` prints the normalised spelling, which parses back to an equal
/// expression with the same [`ParseOptions`].
///
/// ```
/// use belay_cron::CronExpr;
///
/// let expr = CronExpr::parse("0,30 9-17 * * mon-fri")?;
/// assert_eq!(expr.to_string(), "*/30 9-17 * * MON-FRI");
/// assert_eq!(expr, "*/30 9-17 * * 1-5".parse()?);
/// # Ok::<(), belay_cron::CronError>(())
/// ```
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CronExpr {
    pub(crate) seconds: u64,
    pub(crate) minutes: u64,
    pub(crate) hours: u32,
    pub(crate) dom: DomSpec,
    pub(crate) months: u16,
    pub(crate) dow: DowSpec,
    pub(crate) years: YearSet,
    pub(crate) options: ParseOptions,
    /// Per year type (see [`year_type`]): months with at least one matching day.
    month_masks: [u16; 14],
    /// Whether any year in the year set has a matching day.
    satisfiable: bool,
}
