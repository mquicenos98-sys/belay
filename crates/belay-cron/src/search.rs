//! Finding fire times: field-wise search over wall-clock readings, then
//! mapping readings to instants through the zone's offset segments.
//!
//! The local search never steps second by second. It works from the largest
//! unit down: a year whose calendar type has no matching month is skipped
//! whole, a month with no matching day likewise, then the day, hour, minute
//! and second are each advanced to the next set bit of their field. Finding
//! the next match is a few dozen bit operations however far away it is.
//!
//! Time zones split the timeline into *segments* of constant offset. Inside
//! a segment, readings and instants correspond one to one, so the search
//! looks for the next matching reading within the segment's range of
//! readings and converts it. At a segment boundary two things can happen:
//!
//! * clocks jump forward: readings in the gap never occur. If any of them
//!   match, the schedule fires once, at the transition instant, the first
//!   instant after the gap;
//! * clocks go back: the readings of the last hour (or half hour) occur
//!   again. Periodic expressions ([`CronExpr::is_periodic`]) fire on both
//!   passes; all others only on the first, so the second segment starts its
//!   search past the repeated readings.
//!
//! Both directions of iteration implement exactly that set of fire times,
//! so `prev_before(next_after(t)) <= t` always holds.

use std::iter::FusedIterator;

use crate::civil::{civil_from_days, days_from_civil, days_in_month, SECS_PER_DAY};
use crate::expr::CronExpr;
use crate::tz::{Segment, Tz};
use crate::{MAX_YEAR, MIN_YEAR};

/// Naive seconds of the first and last readings the search considers.
const FIRST_READING: i64 = days_from_civil(MIN_YEAR as i64, 1, 1) * SECS_PER_DAY;

const LAST_READING: i64 = days_from_civil(MAX_YEAR as i64 + 1, 1, 1) * SECS_PER_DAY - 1;

fn next_bit(mask: u64, from: u32) -> Option<u32> {
    if from >= 64 {
        return None;
    }
    let rest = mask >> from;
    (rest != 0).then(|| from + rest.trailing_zeros())
}

fn prev_bit(mask: u64, from: u32) -> Option<u32> {
    let rest = if from >= 63 {
        mask
    } else {
        mask & ((2u64 << from) - 1)
    };
    (rest != 0).then(|| 63 - rest.leading_zeros())
}

fn naive(year: i64, month: u32, day: u32, h: u32, m: u32, s: u32) -> i64 {
    days_from_civil(year, month, day) * SECS_PER_DAY
        + i64::from(h) * 3600
        + i64::from(m) * 60
        + i64::from(s)
}

/// Calendar position of a naive reading: `(year, month, day, h, m, s)`.
fn split(reading: i64) -> (i64, u32, u32, u32, u32, u32) {
    let (y, mo, d) = civil_from_days(reading.div_euclid(SECS_PER_DAY));
    let sod = reading.rem_euclid(SECS_PER_DAY) as u32;
    (y, mo, d, sod / 3600, sod / 60 % 60, sod % 60)
}

impl CronExpr {
    /// The first matching reading at or after `from` (naive seconds).
    pub(crate) fn next_reading(&self, from: i64) -> Option<i64> {
        if from > LAST_READING || !self.is_satisfiable() {
            return None;
        }
        let (mut y, mut mo, mut d, mut h, mut mi, mut s) = split(from.max(FIRST_READING));
        loop {
            if !self.years.contains(y) {
                y = self.years.next_from(y)?;
                (mo, d, h, mi, s) = (1, 1, 0, 0, 0);
            }
            let months = self.months_in_year(y);
            let Some(m2) = next_bit(u64::from(months), mo) else {
                (y, mo, d, h, mi, s) = (y + 1, 1, 1, 0, 0, 0);
                continue;
            };
            if m2 != mo {
                (mo, d, h, mi, s) = (m2, 1, 0, 0, 0);
            }
            let Some(d2) = next_bit(u64::from(self.day_mask(y, mo)), d) else {
                (mo, d, h, mi, s) = (mo + 1, 1, 0, 0, 0);
                if mo > 12 {
                    (y, mo) = (y + 1, 1);
                }
                continue;
            };
            if d2 != d {
                (d, h, mi, s) = (d2, 0, 0, 0);
            }
            if let Some((h, mi, s)) = self.next_time_of_day(h, mi, s) {
                return Some(naive(y, mo, d, h, mi, s));
            }
            (d, h, mi, s) = (d + 1, 0, 0, 0);
            if d > days_in_month(y, mo) {
                (mo, d) = (mo + 1, 1);
                if mo > 12 {
                    (y, mo) = (y + 1, 1);
                }
            }
        }
    }

    /// The last matching reading at or before `from` (naive seconds).
    pub(crate) fn prev_reading(&self, from: i64) -> Option<i64> {
        if from < FIRST_READING || !self.is_satisfiable() {
            return None;
        }
        let (mut y, mut mo, mut d, mut h, mut mi, mut s) = split(from.min(LAST_READING));
        loop {
            if !self.years.contains(y) {
                y = self.years.prev_from(y)?;
                (mo, d, h, mi, s) = (12, 31, 23, 59, 59);
            }
            let months = self.months_in_year(y);
            let Some(m2) = prev_bit(u64::from(months), mo) else {
                (y, mo, d, h, mi, s) = (y - 1, 12, 31, 23, 59, 59);
                continue;
            };
            if m2 != mo {
                (mo, d, h, mi, s) = (m2, 31, 23, 59, 59);
            }
            // `d` may be the "end of month" sentinel 31.
            d = d.min(days_in_month(y, mo));
            let Some(d2) = prev_bit(u64::from(self.day_mask(y, mo)), d) else {
                (d, h, mi, s) = (31, 23, 59, 59);
                if mo == 1 {
                    (y, mo) = (y - 1, 12);
                } else {
                    mo -= 1;
                }
                continue;
            };
            if d2 != d {
                (d, h, mi, s) = (d2, 23, 59, 59);
            }
            if let Some((h, mi, s)) = self.prev_time_of_day(h, mi, s) {
                return Some(naive(y, mo, d, h, mi, s));
            }
            (h, mi, s) = (23, 59, 59);
            if d > 1 {
                d -= 1;
            } else {
                d = 31;
                if mo == 1 {
                    (y, mo) = (y - 1, 12);
                } else {
                    mo -= 1;
                }
            }
        }
    }

    /// First matching time of day at or after `h:m:s`, within the same day.
    fn next_time_of_day(&self, mut h: u32, mut m: u32, mut s: u32) -> Option<(u32, u32, u32)> {
        loop {
            let h2 = next_bit(u64::from(self.hours), h)?;
            if h2 != h {
                (h, m, s) = (h2, 0, 0);
            }
            let Some(m2) = next_bit(self.minutes, m) else {
                (h, m, s) = (h + 1, 0, 0);
                continue;
            };
            if m2 != m {
                (m, s) = (m2, 0);
            }
            match next_bit(self.seconds, s) {
                Some(s2) => return Some((h, m, s2)),
                None if m < 59 => (m, s) = (m + 1, 0),
                None => (h, m, s) = (h + 1, 0, 0),
            }
        }
    }

    /// Last matching time of day at or before `h:m:s`, within the same day.
    fn prev_time_of_day(&self, mut h: u32, mut m: u32, mut s: u32) -> Option<(u32, u32, u32)> {
        loop {
            let h2 = prev_bit(u64::from(self.hours), h)?;
            if h2 != h {
                (h, m, s) = (h2, 59, 59);
            }
            let Some(m2) = prev_bit(self.minutes, m) else {
                if h == 0 {
                    return None;
                }
                (h, m, s) = (h - 1, 59, 59);
                continue;
            };
            if m2 != m {
                (m, s) = (m2, 59);
            }
            match prev_bit(self.seconds, s) {
                Some(s2) => return Some((h, m, s2)),
                None if m > 0 => (m, s) = (m - 1, 59),
                None if h > 0 => (h, m, s) = (h - 1, 59, 59),
                None => return None,
            }
        }
    }

    /// The first fire time strictly after `utc` (Unix seconds) in `tz`.
    ///
    /// Returns `None` when the expression never matches again before the end
    /// of 2199 (local time), which includes expressions that can never match
    /// at all such as `0 0 30 2 *`.
    ///
    /// Daylight saving time: a matching reading that the clocks skip fires
    /// once, at the first instant after the jump; several skipped matches
    /// collapse into that one fire. A reading that occurs twice fires only
    /// on its first occurrence, unless the expression
    /// [is periodic](Self::is_periodic), in which case it fires on both.
    ///
    /// ```
    /// use belay_cron::{CronExpr, Tz};
    ///
    /// let ny = Tz::named("America/New_York")?;
    /// let expr = CronExpr::parse("30 2 * * *")?;
    /// // 2025-03-09 02:30 does not exist in New York; the job runs at 03:00 EDT.
    /// let t = expr.next_after(1_741_496_400, &ny).unwrap(); // from 05:00 UTC
    /// assert_eq!(ny.format_rfc3339(t), "2025-03-09T03:00:00-04:00");
    /// # Ok::<(), belay_cron::CronError>(())
    /// ```
    #[must_use]
    pub fn next_after(&self, utc: i64, tz: &Tz) -> Option<i64> {
        let mut t = utc.checked_add(1)?;
        // The segment containing `utc`, not `t`: when `t` is exactly a
        // forward transition, the gap fire at `t` belongs to the segment
        // that ends there.
        let mut seg = tz.segment_at(utc);
        let periodic = self.is_periodic();
        // (from, found): the search from reading `from` returned `found`;
        // reused while later segments start inside [from, found].
        let mut cache: Option<(i64, i64)> = None;
        loop {
            let offset = i64::from(seg.offset);
            let mut from = t.saturating_add(offset);
            if let Some(start) = seg.start {
                if !periodic && start.offset_before > start.offset_after {
                    // Readings below this bound already occurred before the
                    // clocks went back.
                    from = from.max(start.at + i64::from(start.offset_before));
                }
            }
            let reading = match cache {
                Some((f, found)) if f <= from && from <= found => found,
                _ => {
                    let found = self.next_reading(from)?;
                    cache = Some((from, found));
                    found
                }
            };
            let Some(end) = seg.end else {
                return Some(reading - offset);
            };
            if reading < end.at + offset {
                return Some(reading - offset);
            }
            if end.is_forward() && reading < end.at + i64::from(end.offset_after) {
                return Some(end.at);
            }
            // When the match is far away, jump straight to it instead of
            // walking every transition in between. Each instant in
            // [end.at, far] reads at least `from` (the first condition) and
            // less than `reading` (by the choice of `far`), a range in which
            // nothing matches, so neither a regular nor a gap fire is skipped.
            let (min_off, max_off) = tz.offset_bounds();
            let far = reading - i64::from(max_off) - 1;
            if end.at + i64::from(min_off) >= from && far > end.at {
                t = far;
                seg = tz.segment_at(far);
                continue;
            }
            t = end.at;
            seg = Segment {
                offset: end.offset_after,
                start: Some(end),
                end: tz.next_transition(end.at),
            };
        }
    }

    /// The last fire time strictly before `utc` in `tz`, under the same rules
    /// as [`next_after`](Self::next_after). Returns `None` before 1970.
    #[must_use]
    pub fn prev_before(&self, utc: i64, tz: &Tz) -> Option<i64> {
        let mut t = utc.checked_sub(1)?;
        let mut seg = tz.segment_at(t);
        let periodic = self.is_periodic();
        let mut cache: Option<(i64, i64)> = None;
        loop {
            let offset = i64::from(seg.offset);
            let to = t.saturating_add(offset);
            let reading = match cache {
                Some((f, found)) if found <= to && to <= f => found,
                _ => {
                    let found = self.prev_reading(to)?;
                    cache = Some((to, found));
                    found
                }
            };
            let Some(start) = seg.start else {
                return Some(reading - offset);
            };
            let mut lower = start.at + offset;
            if !periodic && start.offset_before > start.offset_after {
                lower = lower.max(start.at + i64::from(start.offset_before));
            }
            if reading >= lower {
                return Some(reading - offset);
            }
            if start.is_forward() && reading >= start.at + i64::from(start.offset_before) {
                return Some(start.at);
            }
            // Mirror of the jump in `next_after`: instants in [far, start.at)
            // read above `reading` and at most `to`, where nothing matches.
            let (min_off, max_off) = tz.offset_bounds();
            let far = reading - i64::from(min_off) + 1;
            if start.at - 1 + i64::from(max_off) <= to && far < start.at - 1 {
                t = far - 1;
                seg = tz.segment_at(t);
                continue;
            }
            t = start.at - 1;
            seg = Segment {
                offset: start.offset_before,
                start: tz.prev_transition(t),
                end: Some(start),
            };
        }
    }

    /// Whether the schedule fires exactly at `utc` in `tz`.
    #[must_use]
    pub fn matches(&self, utc: i64, tz: &Tz) -> bool {
        utc.checked_sub(1)
            .and_then(|before| self.next_after(before, tz))
            == Some(utc)
    }

    /// Fire times strictly after `from`, in order.
    ///
    /// ```
    /// use belay_cron::{CronExpr, Tz};
    ///
    /// let expr = CronExpr::parse("0 9 * * MON-FRI")?;
    /// let tz = Tz::named("Europe/Berlin")?;
    /// // From Friday 2025-01-03T12:00:00Z: Monday, Tuesday, Wednesday at 09:00 CET.
    /// let next: Vec<String> = expr
    ///     .upcoming(1_735_905_600, &tz)
    ///     .take(3)
    ///     .map(|t| tz.format_rfc3339(t))
    ///     .collect();
    /// assert_eq!(next, [
    ///     "2025-01-06T09:00:00+01:00",
    ///     "2025-01-07T09:00:00+01:00",
    ///     "2025-01-08T09:00:00+01:00",
    /// ]);
    /// # Ok::<(), belay_cron::CronError>(())
    /// ```
    pub fn upcoming<'a>(&'a self, from: i64, tz: &'a Tz) -> Upcoming<'a> {
        Upcoming {
            expr: self,
            tz,
            cursor: Some(from),
        }
    }

    /// Fire times strictly before `from`, most recent first.
    pub fn preceding<'a>(&'a self, from: i64, tz: &'a Tz) -> Preceding<'a> {
        Preceding {
            expr: self,
            tz,
            cursor: Some(from),
        }
    }
}

/// Iterator over future fire times; see [`CronExpr::upcoming`].
#[derive(Debug, Clone)]
#[must_use = "iterators are lazy and do nothing unless consumed"]
pub struct Upcoming<'a> {
    expr: &'a CronExpr,
    tz: &'a Tz,
    cursor: Option<i64>,
}
