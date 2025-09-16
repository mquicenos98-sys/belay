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
