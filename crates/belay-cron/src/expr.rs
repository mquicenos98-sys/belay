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

/// Day-of-month field: plain days plus special items.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct DomSpec {
    /// Bits 1-31.
    pub days: u32,
    /// `L`.
    pub last: bool,
    /// Bit `n` set for `L-n` (1-30).
    pub before_last: u32,
    /// Bit `n` set for `nW` (1-31).
    pub nearest: u32,
    /// `LW`.
    pub last_weekday: bool,
}

/// Day-of-week field: plain weekdays plus special items.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct DowSpec {
    /// Bits 0-6, 0 = Sunday.
    pub days: u8,
    /// Bit `d` set for `dL`, the last weekday `d` of the month.
    pub last: u8,
    /// `nth[d]` has bit `n` set for `d#n`.
    pub nth: [u8; 7],
}

/// Years 1970-2199 as a bit set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct YearSet([u64; 4]);

const YEAR_COUNT: u32 = MAX_YEAR - MIN_YEAR + 1;

impl YearSet {
    pub(crate) const ALL: YearSet = {
        let mut words = [u64::MAX; 4];
        // 230 years: the last word keeps its low 230 - 192 = 38 bits.
        words[3] = (1 << (YEAR_COUNT - 192)) - 1;
        YearSet(words)
    };

    pub(crate) const EMPTY: YearSet = YearSet([0; 4]);

    pub(crate) fn insert(&mut self, year: u32) {
        let i = year - MIN_YEAR;
        self.0[(i / 64) as usize] |= 1 << (i % 64);
    }

    pub(crate) fn contains(&self, year: i64) -> bool {
        if !(i64::from(MIN_YEAR)..=i64::from(MAX_YEAR)).contains(&year) {
            return false;
        }
        let i = (year - i64::from(MIN_YEAR)) as u32;
        self.0[(i / 64) as usize] & (1 << (i % 64)) != 0
    }

    pub(crate) fn is_all(&self) -> bool {
        *self == YearSet::ALL
    }

    /// The first year in the set at or after `year`.
    pub(crate) fn next_from(&self, year: i64) -> Option<i64> {
        let start = year.max(i64::from(MIN_YEAR)) - i64::from(MIN_YEAR);
        if start >= i64::from(YEAR_COUNT) {
            return None;
        }
        let start = start as u32;
        let first_word = (start / 64) as usize;
        for w in first_word..4 {
            let mut word = self.0[w];
            if w == first_word {
                word &= u64::MAX << (start % 64);
            }
            if word != 0 {
                let bit = w as u32 * 64 + word.trailing_zeros();
                return Some(i64::from(MIN_YEAR + bit));
            }
        }
        None
    }

    /// The last year in the set at or before `year`.
    pub(crate) fn prev_from(&self, year: i64) -> Option<i64> {
        let end = year.min(i64::from(MAX_YEAR)) - i64::from(MIN_YEAR);
        if end < 0 {
            return None;
        }
        let end = end as u32;
        let last_word = (end / 64) as usize;
        for w in (0..=last_word).rev() {
            let mut word = self.0[w];
            if w == last_word {
                word &= (1u64 << (end % 64 + 1)) - 1;
            }
            if word != 0 {
                let bit = w as u32 * 64 + 63 - word.leading_zeros();
                return Some(i64::from(MIN_YEAR + bit));
            }
        }
        None
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        (MIN_YEAR..=MAX_YEAR).filter(move |&y| self.contains(i64::from(y)))
    }
}

impl DomSpec {
    pub(crate) const ALL: DomSpec = DomSpec {
        days: ALL_DAYS,
        last: false,
        before_last: 0,
        nearest: 0,
        last_weekday: false,
    };

    pub(crate) fn has_specials(&self) -> bool {
        self.last || self.before_last != 0 || self.nearest != 0 || self.last_weekday
    }

    pub(crate) fn is_restricted(&self) -> bool {
        self.days != ALL_DAYS
    }

    /// Matching days of a month with `dim` days whose 1st is weekday `first`.
    fn mask(&self, dim: u32, first: u32) -> u32 {
        let valid = month_days(dim);
        let mut mask = self.days & valid;
        if self.last {
            mask |= 1 << dim;
        }
        for n in bits(u64::from(self.before_last)) {
            if n < dim {
                mask |= 1 << (dim - n);
            }
        }
        for n in bits(u64::from(self.nearest)) {
            if n <= dim {
                mask |= 1 << nearest_weekday(n, dim, first);
            }
        }
        if self.last_weekday {
            let d = match weekday_of(dim, first) {
                6 => dim - 1,
                0 => dim - 2,
                _ => dim,
            };
            mask |= 1 << d;
        }
        mask
    }
}

impl DowSpec {
    pub(crate) const ALL: DowSpec = DowSpec {
        days: ALL_WEEKDAYS,
        last: 0,
        nth: [0; 7],
    };

    pub(crate) fn is_restricted(&self) -> bool {
        self.days != ALL_WEEKDAYS
    }

    fn mask(&self, dim: u32, first: u32) -> u32 {
        let mut mask = 0u32;
        for wd in 0..7u32 {
            // First day of the month falling on weekday `wd`.
            let first_day = (wd + 7 - first) % 7 + 1;
            if self.days & (1 << wd) != 0 {
                let mut d = first_day;
                while d <= dim {
                    mask |= 1 << d;
                    d += 7;
                }
            }
            if self.last & (1 << wd) != 0 {
                mask |= 1 << (first_day + (dim - first_day) / 7 * 7);
            }
            for n in bits(u64::from(self.nth[wd as usize])) {
                let d = first_day + 7 * (n - 1);
                if d <= dim {
                    mask |= 1 << d;
                }
            }
        }
        mask
    }
}

/// Bits 1..=dim.
const fn month_days(dim: u32) -> u32 {
    ((1u64 << (dim + 1)) - 2) as u32
}

const fn weekday_of(day: u32, first: u32) -> u32 {
    (first + day - 1) % 7
}

/// Quartz's `nW`: the Monday-Friday day nearest to `n`, never leaving the
/// month (so `1W` on a Saturday is Monday the 3rd).
fn nearest_weekday(n: u32, dim: u32, first: u32) -> u32 {
    match weekday_of(n, first) {
        6 if n == 1 => 3,
        6 => n - 1,
        0 if n == dim => n - 2,
        0 => n + 1,
        _ => n,
    }
}

/// Indices of the set bits of `mask`, lowest first.
pub(crate) fn bits(mut mask: u64) -> impl Iterator<Item = u32> {
    std::iter::from_fn(move || {
        if mask == 0 {
            return None;
        }
        let b = mask.trailing_zeros();
        mask &= mask - 1;
        Some(b)
    })
}

/// Index 0-13 identifying a year's calendar: `leap * 7 + weekday of Jan 1`.
pub(crate) fn year_type(year: i64) -> usize {
    let jan1 = weekday_from_days(days_from_civil(year, 1, 1));
    usize::from(is_leap_year(year)) * 7 + jan1 as usize
}

/// Days before the 1st of `month` in a year of the given leap-ness.
fn days_before_month(month: u32, leap: bool) -> u32 {
    const CUMULATIVE: [u32; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    CUMULATIVE[month as usize - 1] + u32::from(leap && month > 2)
}

/// Field sets as produced by the parser, before the derived tables exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Fields {
    pub seconds: u64,
    pub minutes: u64,
    pub hours: u32,
    pub dom: DomSpec,
    pub months: u16,
    pub dow: DowSpec,
    pub years: YearSet,
}

impl Default for Fields {
    /// Every field unrestricted except seconds, which default to `0`.
    fn default() -> Self {
        Fields {
            seconds: 1,
            minutes: ALL_MINUTES,
            hours: ALL_HOURS,
            dom: DomSpec::ALL,
            months: ALL_MONTHS,
            dow: DowSpec::ALL,
            years: YearSet::ALL,
        }
    }
}
