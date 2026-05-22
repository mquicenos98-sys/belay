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
            if w == last_word && end % 64 != 63 {
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

impl CronExpr {
    /// Parses an expression in the default (Vixie) dialect.
    ///
    /// Accepts five fields (`minute hour day-of-month month day-of-week`),
    /// six (seconds first) or seven (plus a trailing year), and the
    /// nicknames `@yearly`, `@annually`, `@monthly`, `@weekly`, `@daily`,
    /// `@midnight` and `@hourly`. Fields take `*`, values, names (`JAN`-`DEC`,
    /// `SUN`-`SAT`, any case), ranges, steps (`*/15`, `10-50/5`, `5/10`) and
    /// lists; the day fields also take `?`, `L`, `L-n`, `nW`, `LW` (day of
    /// month), `dL`/`FRIL` and `d#n` (day of week).
    ///
    /// ```
    /// use belay_cron::CronExpr;
    ///
    /// assert!(CronExpr::parse("*/5 9-17 * * MON-FRI").is_ok());
    /// assert!(CronExpr::parse("0 0 L * *").is_ok());
    /// let err = CronExpr::parse("61 * * * *").unwrap_err();
    /// assert_eq!(err.to_string(), "invalid value 61 in minute field (0-59) at column 1");
    /// ```
    pub fn parse(input: &str) -> Result<CronExpr> {
        crate::parse::parse(input, ParseOptions::new())
    }

    /// Parses an expression with explicit options (dialect, day matching).
    pub fn parse_with(input: &str, options: ParseOptions) -> Result<CronExpr> {
        crate::parse::parse(input, options)
    }

    /// Assembles an expression from parsed field sets.
    pub(crate) fn from_fields(fields: Fields, options: ParseOptions) -> CronExpr {
        let Fields {
            seconds,
            minutes,
            hours,
            mut dom,
            months,
            mut dow,
            years,
        } = fields;
        // A field that already covers every day makes its specials redundant;
        // dropping them keeps equality and printing canonical.
        if !dom.is_restricted() {
            dom = DomSpec::ALL;
        }
        if !dow.is_restricted() {
            dow = DowSpec::ALL;
        }
        let mut expr = CronExpr {
            seconds,
            minutes,
            hours,
            dom,
            months,
            dow,
            years,
            options,
            month_masks: [0; 14],
            satisfiable: false,
        };
        for ty in 0..14 {
            let leap = ty >= 7;
            let jan1 = (ty % 7) as u32;
            let mut mask = 0u16;
            for month in bits(u64::from(months)) {
                let first = (jan1 + days_before_month(month, leap)) % 7;
                let dim = match month {
                    2 if leap => 29,
                    2 => 28,
                    m => days_in_month(2001, m),
                };
                if expr.day_mask_for(dim, first) != 0 {
                    mask |= 1 << month;
                }
            }
            expr.month_masks[ty] = mask;
        }
        expr.satisfiable = if expr.years.is_all() {
            expr.month_masks.iter().any(|&m| m != 0)
        } else {
            expr.years
                .iter()
                .any(|y| expr.months_in_year(i64::from(y)) != 0)
        };
        expr
    }

    /// Matching days of a month (bits 1-31), combining the two day fields.
    fn day_mask_for(&self, dim: u32, first: u32) -> u32 {
        match (self.dom.is_restricted(), self.dow.is_restricted()) {
            (false, false) => month_days(dim),
            (true, false) => self.dom.mask(dim, first),
            (false, true) => self.dow.mask(dim, first),
            (true, true) => {
                let dom = self.dom.mask(dim, first);
                let dow = self.dow.mask(dim, first);
                match self.options.day_match() {
                    crate::DayMatch::Either => dom | dow,
                    crate::DayMatch::Both => dom & dow,
                }
            }
        }
    }

    /// Matching days of `month` in `year` (bits 1-31).
    pub(crate) fn day_mask(&self, year: i64, month: u32) -> u32 {
        let first = weekday_from_days(days_from_civil(year, month, 1));
        self.day_mask_for(days_in_month(year, month), first)
    }

    /// Months of `year` that contain a matching day (bits 1-12), or 0 when
    /// the year field excludes the year.
    pub(crate) fn months_in_year(&self, year: i64) -> u16 {
        if !self.years.contains(year) {
            return 0;
        }
        self.month_masks[year_type(year)]
    }

    /// The options the expression was parsed with.
    #[must_use]
    pub fn options(&self) -> ParseOptions {
        self.options
    }

    /// Whether the expression uses a seconds field other than a plain `0`.
    #[must_use]
    pub fn has_seconds(&self) -> bool {
        self.seconds != 1
    }

    /// Whether the expression restricts years.
    #[must_use]
    pub fn has_year(&self) -> bool {
        !self.years.is_all()
    }

    /// Whether the schedule is *periodic*: it fires more than once an hour
    /// (several seconds or minutes) or every hour (`*` hours).
    ///
    /// Periodic schedules follow the wall clock through a repeated hour
    /// when clocks go back and fire in both passes, like cronie's wildcard
    /// jobs; other schedules fire only on the first pass. See
    /// [`next_after`](Self::next_after).
    #[must_use]
    pub fn is_periodic(&self) -> bool {
        self.seconds.count_ones() > 1 || self.minutes.count_ones() > 1 || self.hours == ALL_HOURS
    }

    /// Whether any date in the year range can ever match. `0 0 30 2 *` and
    /// `0 0 0 29 2 * 2025` cannot; iterating them returns `None` at once.
    #[must_use]
    pub fn is_satisfiable(&self) -> bool {
        self.satisfiable
    }

    /// Whether a wall-clock reading matches every field. Time zone rules for
    /// skipped and repeated readings are not involved; see
    /// [`matches`](Self::matches) for that.
    ///
    /// ```
    /// use belay_cron::{CronExpr, LocalDateTime};
    ///
    /// let expr = CronExpr::parse("0 12 L * *")?;
    /// assert!(expr.matches_local(&LocalDateTime::new(2024, 2, 29, 12, 0, 0).unwrap()));
    /// assert!(!expr.matches_local(&LocalDateTime::new(2024, 2, 28, 12, 0, 0).unwrap()));
    /// # Ok::<(), belay_cron::CronError>(())
    /// ```
    #[must_use]
    pub fn matches_local(&self, local: &LocalDateTime) -> bool {
        let year = i64::from(local.year());
        self.seconds & (1 << local.second()) != 0
            && self.minutes & (1 << local.minute()) != 0
            && self.hours & (1 << local.hour()) != 0
            && self.months & (1 << local.month()) != 0
            && self.years.contains(year)
            && self.day_mask(year, local.month()) & (1 << local.day()) != 0
    }
}

impl FromStr for CronExpr {
    type Err = CronError;

    fn from_str(s: &str) -> Result<CronExpr> {
        CronExpr::parse(s)
    }
}

impl fmt::Debug for CronExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut t = f.debug_tuple("CronExpr");
        t.field(&self.to_string());
        if self.options != ParseOptions::default() {
            t.field(&self.options);
        }
        t.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CronExpr;

    fn days(expr: &str, year: i64, month: u32) -> Vec<u32> {
        let e = CronExpr::parse(expr).unwrap();
        bits(u64::from(e.day_mask(year, month))).collect()
    }

    #[test]
    fn last_day_and_offsets_follow_month_length() {
        assert_eq!(days("0 0 L * *", 2024, 2), vec![29]);
        assert_eq!(days("0 0 L * *", 2025, 2), vec![28]);
        assert_eq!(days("0 0 L-2 * *", 2025, 4), vec![28]);
        assert_eq!(days("0 0 1,L * *", 2025, 1), vec![1, 31]);
    }

    #[test]
    fn nearest_weekday_stays_inside_the_month() {
        // 2025-03-01 is a Saturday: 1W moves forward to Monday the 3rd.
        assert_eq!(days("0 0 1W * *", 2025, 3), vec![3]);
        // 2025-03-15 is a Saturday: 15W is Friday the 14th.
        assert_eq!(days("0 0 15W * *", 2025, 3), vec![14]);
        // 2025-03-16 is a Sunday: 16W is Monday the 17th.
        assert_eq!(days("0 0 16W * *", 2025, 3), vec![17]);
        // 2025-08-31 is a Sunday: 31W moves back to Friday the 29th.
        assert_eq!(days("0 0 31W * *", 2025, 8), vec![29]);
        // 31W never matches in a 30-day month.
        assert_eq!(days("0 0 31W * *", 2025, 4), Vec::<u32>::new());
    }

    #[test]
    fn last_weekday_of_month() {
        // 2025-05-31 is a Saturday, 2025-08-31 a Sunday, 2025-07-31 a Thursday.
        assert_eq!(days("0 0 LW * *", 2025, 5), vec![30]);
        assert_eq!(days("0 0 LW * *", 2025, 8), vec![29]);
        assert_eq!(days("0 0 LW * *", 2025, 7), vec![31]);
    }

    #[test]
    fn weekday_specials() {
        // Fridays of 2025-01: 3, 10, 17, 24, 31.
        assert_eq!(days("0 0 * * FRI", 2025, 1), vec![3, 10, 17, 24, 31]);
        assert_eq!(days("0 0 * * FRIL", 2025, 1), vec![31]);
        assert_eq!(days("0 0 * * 5#2", 2025, 1), vec![10]);
        assert_eq!(days("0 0 * * 5#5", 2025, 1), vec![31]);
        assert_eq!(days("0 0 * * 5#5", 2025, 2), Vec::<u32>::new());
        assert_eq!(days("0 0 * * MON#1,MON#3", 2025, 9), vec![1, 15]);
    }

    #[test]
    fn restricted_day_fields_combine_with_or_by_default() {
        // Day 13 or any Friday, January 2025.
        assert_eq!(days("0 0 13 * FRI", 2025, 1), vec![3, 10, 13, 17, 24, 31]);
        let both = CronExpr::parse_with(
            "0 0 13 * FRI",
            ParseOptions::new().with_day_match(crate::DayMatch::Both),
        )
        .unwrap();
        // Friday the 13th: June 2025.
        assert_eq!(both.day_mask(2025, 1), 0);
        assert_eq!(both.day_mask(2025, 6), 1 << 13);
    }

    #[test]
    fn unrestricted_field_defers_to_the_other() {
        assert_eq!(days("0 0 1 * *", 2025, 1), vec![1]);
        assert_eq!(days("0 0 * * 0", 2025, 1), vec![5, 12, 19, 26]);
        assert_eq!(days("0 0 ? * 0", 2025, 1), vec![5, 12, 19, 26]);
        // A day-of-week list covering the whole week is unrestricted.
        assert_eq!(days("0 0 1 * 0-6", 2025, 1), vec![1]);
    }

    #[test]
    fn impossible_dates_are_recognised_as_unsatisfiable() {
        for never in [
            "0 0 30 2 *",
            "0 0 31 4,6,9,11 *",
            "0 0 0 29 2 * 2025",
            "0 0 0 31 2 ? 2025-2030",
        ] {
            assert!(!CronExpr::parse(never).unwrap().is_satisfiable(), "{never}");
        }
        for sometimes in ["0 0 29 2 *", "0 0 0 29 2 * 2028", "0 0 31 * *", "0 0 L * *"] {
            assert!(
                CronExpr::parse(sometimes).unwrap().is_satisfiable(),
                "{sometimes}"
            );
        }
        let friday_29th_feb = CronExpr::parse_with(
            "0 0 29 2 FRI",
            ParseOptions::new().with_day_match(crate::DayMatch::Both),
        )
        .unwrap();
        assert!(friday_29th_feb.is_satisfiable());
    }

    #[test]
    fn periodicity_follows_the_documented_rule() {
        for periodic in [
            "* * * * *",
            "*/15 * * * *",
            "0 * * * *",
            "0,30 1 * * *",
            "*/10 0 1 * * *",
        ] {
            assert!(
                CronExpr::parse(periodic).unwrap().is_periodic(),
                "{periodic}"
            );
        }
        for fixed in ["30 1 * * *", "0 */2 * * *", "0 1-5 * * *", "15 0 1 * * *"] {
            assert!(!CronExpr::parse(fixed).unwrap().is_periodic(), "{fixed}");
        }
    }

    #[test]
    fn year_set_bounds() {
        assert!(YearSet::ALL.contains(1970));
        assert!(YearSet::ALL.contains(2199));
        assert!(!YearSet::ALL.contains(2200));
        assert!(!YearSet::ALL.contains(1969));
        assert_eq!(YearSet::ALL.iter().count(), 230);
        let mut s = YearSet::EMPTY;
        s.insert(2025);
        s.insert(2199);
        assert_eq!(s.iter().collect::<Vec<_>>(), vec![2025, 2199]);
    }

    #[test]
    fn year_set_jumps_to_neighbouring_members() {
        let mut s = YearSet::EMPTY;
        for y in [1970, 2033, 2034, 2100, 2199] {
            s.insert(y);
        }
        assert_eq!(s.next_from(1900), Some(1970));
        assert_eq!(s.next_from(1971), Some(2033));
        assert_eq!(s.next_from(2034), Some(2034));
        assert_eq!(s.next_from(2035), Some(2100));
        assert_eq!(s.next_from(2101), Some(2199));
        assert_eq!(s.next_from(2200), None);
        assert_eq!(s.prev_from(2300), Some(2199));
        assert_eq!(s.prev_from(2198), Some(2100));
        assert_eq!(s.prev_from(2033), Some(2033));
        assert_eq!(s.prev_from(2032), Some(1970));
        assert_eq!(s.prev_from(1969), None);
        // Word boundaries sit at offsets 64 and 128 from 1970 (2034, 2098).
        let mut w = YearSet::EMPTY;
        w.insert(2033);
        w.insert(2098);
        assert_eq!(w.next_from(2034), Some(2098));
        assert_eq!(w.prev_from(2097), Some(2033));
        assert_eq!(YearSet::ALL.next_from(2150), Some(2150));
        assert_eq!(YearSet::ALL.prev_from(2161), Some(2161));
        assert_eq!(YearSet::EMPTY.next_from(1970), None);
        assert_eq!(YearSet::EMPTY.prev_from(2199), None);
    }

    #[test]
    fn year_types_distinguish_leap_years_and_start_days() {
        assert_eq!(year_type(2025), 3); // common year starting Wednesday
        assert_eq!(year_type(2024), 7 + 1); // leap year starting Monday
        assert_eq!(year_type(2000), 7 + 6);
        let types: std::collections::HashSet<usize> = (2001..=2028).map(year_type).collect();
        assert_eq!(types.len(), 14);
    }

    #[test]
    fn debug_shows_the_normalised_text() {
        let e = CronExpr::parse("*/15 * * * *").unwrap();
        assert_eq!(format!("{e:?}"), "CronExpr(\"*/15 * * * *\")");
    }
}
