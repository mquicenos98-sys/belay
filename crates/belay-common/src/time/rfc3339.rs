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
                millis += i64::from(b - b'0') * [100, 10, 1][index];
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

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Timestamp {
        match Timestamp::parse_rfc3339(s) {
            Ok(t) => t,
            Err(e) => panic!("{s:?} failed to parse: {e}"),
        }
    }

    fn err(s: &str) -> String {
        match Timestamp::parse_rfc3339(s) {
            Ok(t) => panic!("{s:?} unexpectedly parsed as {t}"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn formats_with_fixed_millisecond_precision() {
        assert_eq!(
            Timestamp::from_millis(1_741_064_767_089).to_rfc3339(),
            "2025-03-04T05:06:07.089Z"
        );
        assert_eq!(
            Timestamp::from_millis(1_741_064_767_000).to_rfc3339(),
            "2025-03-04T05:06:07.000Z"
        );
        assert_eq!(
            Timestamp::from_millis(-1).to_rfc3339(),
            "1969-12-31T23:59:59.999Z"
        );
        assert_eq!(
            Timestamp::from_secs(-2_208_988_800).to_rfc3339(),
            "1900-01-01T00:00:00.000Z"
        );
    }

    #[test]
    fn parses_utc_and_offsets() {
        assert_eq!(p("2025-03-04T05:06:07.089Z").as_millis(), 1_741_064_767_089);
        assert_eq!(p("2025-03-04T05:06:07Z").as_millis(), 1_741_064_767_000);
        assert_eq!(p("2025-03-04T07:06:07+02:00"), p("2025-03-04T05:06:07Z"));
        assert_eq!(p("2025-03-04T00:36:07-04:30"), p("2025-03-04T05:06:07Z"));
        assert_eq!(p("2025-03-04T05:06:07-00:00"), p("2025-03-04T05:06:07Z"));
        assert_eq!(p("2025-03-04T05:06:07+00:00"), p("2025-03-04T05:06:07Z"));
    }

    #[test]
    fn accepts_lowercase_separators_and_space() {
        let expected = p("2025-03-04T05:06:07Z");
        assert_eq!(p("2025-03-04t05:06:07z"), expected);
        assert_eq!(p("2025-03-04 05:06:07Z"), expected);
    }

    #[test]
    fn fractional_seconds_are_truncated_to_milliseconds() {
        assert_eq!(p("1970-01-01T00:00:00.1Z").as_millis(), 100);
        assert_eq!(p("1970-01-01T00:00:00.12Z").as_millis(), 120);
        assert_eq!(p("1970-01-01T00:00:00.123Z").as_millis(), 123);
        assert_eq!(p("1970-01-01T00:00:00.1239Z").as_millis(), 123);
        assert_eq!(p("1970-01-01T00:00:00.999999999Z").as_millis(), 999);
        assert_eq!(p("1970-01-01T00:00:00.000000001Z").as_millis(), 0);
    }

    #[test]
    fn rejects_more_than_nine_fractional_digits() {
        assert_eq!(
            err("1970-01-01T00:00:00.1234567891Z"),
            "invalid timestamp: expected at most 9 fractional digits at byte 29"
        );
        assert_eq!(
            err("1970-01-01T00:00:00.Z"),
            "invalid timestamp: expected fractional digits after '.' at byte 20"
        );
    }

    #[test]
    fn offsets_can_cross_day_month_and_year_boundaries() {
        assert_eq!(
            p("2024-12-31T23:30:00-01:00").to_rfc3339(),
            "2025-01-01T00:30:00.000Z"
        );
        assert_eq!(
            p("2025-01-01T00:15:00+05:30").to_rfc3339(),
            "2024-12-31T18:45:00.000Z"
        );
        assert_eq!(
            p("2024-03-01T00:00:00+00:01").to_rfc3339(),
            "2024-02-29T23:59:00.000Z"
        );
        assert_eq!(
            p("2023-03-01T00:00:00+00:01").to_rfc3339(),
            "2023-02-28T23:59:00.000Z"
        );
    }

    #[test]
    fn leap_days_are_validated_per_year() {
        assert!(Timestamp::parse_rfc3339("2024-02-29T00:00:00Z").is_ok());
        assert!(Timestamp::parse_rfc3339("2000-02-29T00:00:00Z").is_ok());
        assert_eq!(err("1900-02-29T00:00:00Z"), "day 29 is out of range (1-28)");
        assert_eq!(err("2023-04-31T00:00:00Z"), "day 31 is out of range (1-30)");
    }

    #[test]
    fn negative_timestamps_before_1970() {
        assert_eq!(p("1969-12-31T23:59:59.999Z").as_millis(), -1);
        assert_eq!(p("1960-06-15T12:00:00Z").as_millis(), -301_233_600_000);
        assert_eq!(p("0000-01-01T00:00:00Z"), Timestamp::MIN);
        assert_eq!(p("9999-12-31T23:59:59.999999Z"), Timestamp::MAX);
    }

    #[test]
    fn offsets_pushing_outside_supported_range_are_rejected() {
        assert_eq!(
            Timestamp::parse_rfc3339("0000-01-01T00:00:00+00:01"),
            Err(TimeError::OutOfRange)
        );
        assert_eq!(
            Timestamp::parse_rfc3339("9999-12-31T23:59:59-00:01"),
            Err(TimeError::OutOfRange)
        );
    }

    #[test]
    fn rejects_field_values_out_of_range() {
        assert_eq!(
            err("2025-13-01T00:00:00Z"),
            "month 13 is out of range (1-12)"
        );
        assert_eq!(
            err("2025-00-01T00:00:00Z"),
            "month 0 is out of range (1-12)"
        );
        assert_eq!(err("2025-01-00T00:00:00Z"), "day 0 is out of range (1-31)");
        assert_eq!(
            err("2025-01-01T24:00:00Z"),
            "hour 24 is out of range (0-23)"
        );
        assert_eq!(
            err("2025-01-01T00:60:00Z"),
            "minute 60 is out of range (0-59)"
        );
        assert_eq!(
            err("2025-01-01T00:00:61Z"),
            "second 61 is out of range (0-59)"
        );
        assert_eq!(
            err("2025-01-01T00:00:00+24:00"),
            "offset hour 24 is out of range (0-23)"
        );
        assert_eq!(
            err("2025-01-01T00:00:00+01:60"),
            "offset minute 60 is out of range (0-59)"
        );
        assert_eq!(
            Timestamp::parse_rfc3339("2016-12-31T23:59:60Z"),
            Err(TimeError::LeapSecond)
        );
    }

    #[test]
    fn rejects_malformed_syntax_with_position() {
        assert_eq!(
            err(""),
            "invalid timestamp: expected 4-digit year at byte 0"
        );
        assert_eq!(
            err("25-03-04T05:06:07Z"),
            "invalid timestamp: expected 4-digit year at byte 2"
        );
        assert_eq!(
            err("2025/03/04T05:06:07Z"),
            "invalid timestamp: expected '-' after year at byte 4"
        );
        assert_eq!(
            err("2025-3-04T05:06:07Z"),
            "invalid timestamp: expected 2-digit month at byte 6"
        );
        assert_eq!(
            err("2025-03-04_05:06:07Z"),
            "invalid timestamp: expected 'T' between date and time at byte 10"
        );
        assert_eq!(
            err("2025-03-04T05:06Z"),
            "invalid timestamp: expected ':' after minute at byte 16"
        );
        assert_eq!(
            err("2025-03-04T05:06:07"),
            "invalid timestamp: expected 'Z' or a numeric offset at byte 19"
        );
        assert_eq!(
            err("2025-03-04T05:06:07+0200"),
            "invalid timestamp: expected ':' in offset at byte 22"
        );
        assert_eq!(
            err("2025-03-04T05:06:07Z "),
            "invalid timestamp: expected end of input at byte 20"
        );
        assert_eq!(
            err("+2025-03-04T05:06:07Z"),
            "invalid timestamp: expected 4-digit year at byte 0"
        );
    }

    #[test]
    fn non_ascii_input_is_rejected_without_panicking() {
        assert!(Timestamp::parse_rfc3339("2025-03-04T05:06:07\u{00e9}").is_err());
        assert!(Timestamp::parse_rfc3339("\u{0662}025-03-04T05:06:07Z").is_err());
    }

    #[test]
    fn format_then_parse_round_trips() {
        // Sample the range with a fixed stride that is coprime with the day
        // length so that every time-of-day component varies.
        let mut ms = Timestamp::MIN.as_millis();
        let step = 97_777_777_777;
        while ms <= Timestamp::MAX.as_millis() {
            let t = Timestamp::from_millis(ms);
            assert_eq!(p(&t.to_rfc3339()), t);
            ms += step;
        }
    }

    #[test]
    fn parse_date_only_accepts_calendar_dates() {
        assert_eq!(
            Timestamp::parse_date("2024-02-29").map(|t| t.to_rfc3339()),
            Ok("2024-02-29T00:00:00.000Z".to_owned())
        );
        assert_eq!(
            Timestamp::parse_date("1969-12-31").map(|t| t.as_secs()),
            Ok(-86_400)
        );
        assert!(Timestamp::parse_date("2023-02-29").is_err());
        assert!(Timestamp::parse_date("2024-02-29T00:00:00Z").is_err());
    }
}
