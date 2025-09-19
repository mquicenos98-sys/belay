//! HTTP dates (RFC 9110 §5.6.7) in the IMF-fixdate form
//! `Sun, 06 Nov 1994 08:49:37 GMT`.
//!
//! Calendar conversion uses Howard Hinnant's `days_from_civil` /
//! `civil_from_days` algorithms, which are exact for the proleptic
//! Gregorian calendar. The obsolete RFC 850 and asctime forms are not
//! accepted by the parser; no current implementation sends them.

use std::cell::RefCell;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Formats `time` as an IMF-fixdate. Times before the Unix epoch are
/// clamped to it.
///
/// ```
/// use std::time::{Duration, UNIX_EPOCH};
/// use belay_http::fmt_http_date;
///
/// let t = UNIX_EPOCH + Duration::from_secs(784_111_777);
/// assert_eq!(fmt_http_date(t), "Sun, 06 Nov 1994 08:49:37 GMT");
/// ```
pub fn fmt_http_date(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let rem = secs % 86_400;
    // i64 comfortably holds any day count derived from a u64 of seconds.
    let (year, month, day) = civil_from_days(days as i64);
    // 1970-01-01 was a Thursday (index 3 in a Monday-first table).
    let weekday = WEEKDAYS[((days + 3) % 7) as usize];
    format!(
        "{weekday}, {day:02} {} {year:04} {:02}:{:02}:{:02} GMT",
        MONTHS[(month - 1) as usize],
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Parses an IMF-fixdate. Returns `None` for any other format, impossible
/// dates, or dates before 1970.
pub fn parse_http_date(s: &str) -> Option<SystemTime> {
    let b = s.as_bytes();
    if b.len() != 29 || &b[3..5] != b", " || &b[25..] != b" GMT" {
        return None;
    }
    if !WEEKDAYS.iter().any(|d| d.as_bytes() == &b[..3]) {
        return None;
    }
    let day = digits(&b[5..7])?;
    let month = MONTHS.iter().position(|m| m.as_bytes() == &b[8..11])? as u32 + 1;
    let year = digits(&b[12..16])?;
    let (hour, minute, second) = (
        digits(&b[17..19])?,
        digits(&b[20..22])?,
        digits(&b[23..25])?,
    );
    let separators_ok =
        b[7] == b' ' && b[11] == b' ' && b[16] == b' ' && b[19] == b':' && b[22] == b':';
    if !separators_ok
        || year < 1970
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let days = days_from_civil(i64::from(year), month, day);
    // A leap second (:60) is folded into the following second.
    let secs =
        days as u64 * 86_400 + u64::from(hour) * 3600 + u64::from(minute) * 60 + u64::from(second);
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

/// The `Date` value for the current second, formatted at most once per
/// second per thread.
pub(crate) fn now_http_date() -> String {
    thread_local! {
        static CACHE: RefCell<(u64, String)> = const { RefCell::new((u64::MAX, String::new())) };
    }
    let now = SystemTime::now();
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.0 != secs {
            *cache = (secs, fmt_http_date(now));
        }
        cache.1.clone()
    })
}

fn digits(b: &[u8]) -> Option<u32> {
    b.iter().try_fold(0u32, |acc, &c| {
        c.is_ascii_digit().then(|| acc * 10 + u32::from(c - b'0'))
    })
}

fn is_leap(year: u32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let month = i64::from(month);
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn formats_known_instants() {
        assert_eq!(fmt_http_date(at(0)), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(
            fmt_http_date(at(951_782_400)),
            "Tue, 29 Feb 2000 00:00:00 GMT"
        );
        assert_eq!(
            fmt_http_date(at(1_709_251_199)),
            "Thu, 29 Feb 2024 23:59:59 GMT"
        );
        assert_eq!(
            fmt_http_date(at(4_102_444_800)),
            "Fri, 01 Jan 2100 00:00:00 GMT"
        );
    }

    #[test]
    fn clamps_pre_epoch_times() {
        let before = UNIX_EPOCH - Duration::from_secs(10);
        assert_eq!(fmt_http_date(before), "Thu, 01 Jan 1970 00:00:00 GMT");
    }

    #[test]
    fn parse_inverts_format_across_a_range_of_days() {
        // One sample every ~37 days for 150 years covers every month,
        // weekday and leap-year rule.
        let mut secs = 0u64;
        while secs < 150 * 365 * 86_400 {
            let text = fmt_http_date(at(secs));
            assert_eq!(parse_http_date(&text), Some(at(secs)), "{text}");
            secs += 37 * 86_400 + 3_607;
        }
    }

    #[test]
    fn rejects_other_formats_and_impossible_dates() {
        for bad in [
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
            "Sun, 06 Nov 1994 08:49:37 UTC",
            "Sun, 31 Apr 2024 00:00:00 GMT",
            "Sun, 29 Feb 2023 00:00:00 GMT",
            "Sun, 06 Nov 1994 24:00:00 GMT",
            "Xyz, 06 Nov 1994 08:49:37 GMT",
            "Sun, 06 Nov 1969 08:49:37 GMT",
            "Sun, 0a Nov 1994 08:49:37 GMT",
            "",
        ] {
            assert_eq!(parse_http_date(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn cached_date_has_the_fixed_length() {
        let first = now_http_date();
        assert_eq!(first.len(), 29);
        assert!(parse_http_date(&first).is_some());
    }
}
