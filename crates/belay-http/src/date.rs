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
