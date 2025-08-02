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
