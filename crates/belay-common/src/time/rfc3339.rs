//! RFC 3339 (`date-time` production of section 5.6) text form of [`Timestamp`].
//!
//! Output is always UTC with exactly three fractional digits so that the
//! strings have a fixed width and sort like the instants they denote. Input
//! is more permissive, as the RFC allows: any offset, a space instead of `T`,
//! lowercase `t`/`z`, and up to nine fractional digits.

use std::fmt;

use super::{check_field, days_from_civil, days_in_month, TimeError, Timestamp};
use super::{MILLIS_PER_DAY, MILLIS_PER_SEC};

pub(super) fn write(ts: Timestamp, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let c = ts.to_civil();
    // The clamped range guarantees 0 <= year <= 9999, so `{:04}` is exact.
    write!(
        f,
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        c.year, c.month, c.day, c.hour, c.minute, c.second, c.millis
    )
}

/// Byte cursor that reports failures with the offset where they happened.
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(s: &'a str) -> Self {
        Cursor {
            bytes: s.as_bytes(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn syntax(&self, expected: &'static str) -> TimeError {
        TimeError::Syntax {
            pos: self.pos,
            expected,
        }
    }

    /// Reads exactly `n` ASCII digits.
    fn digits(&mut self, n: usize, expected: &'static str) -> Result<u32, TimeError> {
        let mut value = 0u32;
        for _ in 0..n {
            match self.peek() {
                Some(b @ b'0'..=b'9') => {
                    value = value * 10 + u32::from(b - b'0');
                    self.pos += 1;
                }
                _ => return Err(self.syntax(expected)),
            }
        }
        Ok(value)
    }

    fn expect(&mut self, byte: u8, expected: &'static str) -> Result<(), TimeError> {
        if self.peek() == Some(byte) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.syntax(expected))
        }
    }

    fn finish(&self) -> Result<(), TimeError> {
        if self.pos == self.bytes.len() {
            Ok(())
        } else {
            Err(self.syntax("end of input"))
        }
    }
}

/// Parses `YYYY-MM-DD` and returns days since the epoch.
fn date(cur: &mut Cursor<'_>) -> Result<i64, TimeError> {
    let year = cur.digits(4, "4-digit year")? as i32;
    cur.expect(b'-', "'-' after year")?;
    let month = cur.digits(2, "2-digit month")? as u8;
    check_field("month", i64::from(month), 1, 12)?;
    cur.expect(b'-', "'-' after month")?;
    let day = cur.digits(2, "2-digit day")? as u8;
    check_field(
        "day",
        i64::from(day),
        1,
        i64::from(days_in_month(year, month)),
    )?;
    Ok(days_from_civil(year, month, day))
}

pub(super) fn parse_date_only(s: &str) -> Result<Timestamp, TimeError> {
    let mut cur = Cursor::new(s);
    let days = date(&mut cur)?;
    cur.finish()?;
    // Years are 4 digits, so every valid date is inside the supported range.
    Ok(Timestamp(days * MILLIS_PER_DAY))
}

pub(super) fn parse(s: &str) -> Result<Timestamp, TimeError> {
    let mut cur = Cursor::new(s);
    let days = date(&mut cur)?;

    match cur.peek() {
        Some(b'T' | b't' | b' ') => cur.pos += 1,
        _ => return Err(cur.syntax("'T' between date and time")),
    }

    let hour = cur.digits(2, "2-digit hour")?;
    check_field("hour", i64::from(hour), 0, 23)?;
    cur.expect(b':', "':' after hour")?;
    let minute = cur.digits(2, "2-digit minute")?;
    check_field("minute", i64::from(minute), 0, 59)?;
    cur.expect(b':', "':' after minute")?;
    let second = cur.digits(2, "2-digit second")?;
    if second == 60 {
        return Err(TimeError::LeapSecond);
    }
    check_field("second", i64::from(second), 0, 59)?;

    let mut millis = 0i64;
    if cur.peek() == Some(b'.') {
        cur.pos += 1;
        let start = cur.pos;
        while let Some(b @ b'0'..=b'9') = cur.peek() {
            let index = cur.pos - start;
            if index >= 9 {
                return Err(cur.syntax("at most 9 fractional digits"));
            }
            // Only the first three digits matter; the rest are truncated.
            if index < 3 {
                millis = millis * 10 + i64::from(b - b'0');
            }
            cur.pos += 1;
        }
        if cur.pos == start {
            return Err(cur.syntax("fractional digits after '.'"));
        }
    }

    let offset_minutes = match cur.peek() {
        Some(b'Z' | b'z') => {
            cur.pos += 1;
            0
        }
        Some(sign @ (b'+' | b'-')) => {
            cur.pos += 1;
            let oh = cur.digits(2, "2-digit offset hour")?;
            check_field("offset hour", i64::from(oh), 0, 23)?;
            cur.expect(b':', "':' in offset")?;
            let om = cur.digits(2, "2-digit offset minute")?;
            check_field("offset minute", i64::from(om), 0, 59)?;
            // "-00:00" means "offset unknown" (RFC 3339 4.3); the instant is
            // still the UTC one, which is all a timestamp records.
            let total = i64::from(oh * 60 + om);
            if sign == b'-' {
                -total
            } else {
                total
            }
        }
        _ => return Err(cur.syntax("'Z' or a numeric offset")),
    };
    cur.finish()?;

    let secs_of_day = i64::from(hour * 3600 + minute * 60 + second);
    let local_ms = days * MILLIS_PER_DAY + secs_of_day * MILLIS_PER_SEC + millis;
    let utc_ms = local_ms - offset_minutes * 60_000;
    Timestamp::try_from_millis(utc_ms).ok_or(TimeError::OutOfRange)
}
