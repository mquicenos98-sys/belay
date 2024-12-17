//! JSON encodings for the belay-common types that belay-json cannot know
//! about (the orphan rule keeps `ToJson` off `Timestamp`, `Duration` and
//! `BackoffPolicy`), plus the extension traits every impl in this crate uses.
//!
//! | type | JSON | example |
//! |---|---|---|
//! | `Timestamp` | RFC 3339 string, UTC, milliseconds | `"2025-03-04T05:06:07.089Z"` |
//! | `Duration` | compact duration string | `"30s"`, `"1h30m"`, `"250ms"` |
//! | `BackoffPolicy` | backoff text form | `"exp:1s*2..10m~full"` |
//!
//! Timestamps are accepted with any RFC 3339 offset and normalised to UTC.
//! Durations are accepted only as strings: a bare JSON number is rejected,
//! because `30000` could mean seconds or milliseconds depending on who wrote
//! it, and the duration grammar already reads a unitless *string* (`"90"`) as
//! seconds. The error message says how to spell it.
//!
//! Downstream crates use these helpers for their own persisted types, so the
//! whole system shares one wire format.

use std::io;
use std::str::FromStr;
use std::time::Duration;

use belay_common::{format_duration, parse_duration, BackoffPolicy, Timestamp};
use belay_json::{FromJsonError, ObjectBuilder, ObjectReader, Value};

use crate::error::{quote, ParseError};

const DURATION_EXPECTATION: &str = r#"duration string such as "30s" or "1h30m""#;

/// Encodes a timestamp as its RFC 3339 string.
#[must_use]
pub fn timestamp_to_json(ts: Timestamp) -> Value {
    Value::String(ts.to_rfc3339())
}

/// Decodes an RFC 3339 timestamp string.
///
/// ```
/// use belay_json::json;
/// use belay_proto::wire::timestamp_from_json;
///
/// let t = timestamp_from_json(&json!("2025-03-04T10:06:07+05:00")).unwrap();
/// assert_eq!(t.to_rfc3339(), "2025-03-04T05:06:07.000Z");
/// let err = timestamp_from_json(&json!(1741064767)).unwrap_err();
/// assert_eq!(err.message(), "expected RFC 3339 timestamp string, found 1741064767");
/// ```
pub fn timestamp_from_json(value: &Value) -> Result<Timestamp, FromJsonError> {
    let s = value
        .as_str()
        .ok_or_else(|| FromJsonError::expected("RFC 3339 timestamp string", value))?;
    parse_timestamp(s).map_err(FromJsonError::new)
}

/// Parses RFC 3339 text with an error message in this crate's style.
pub(crate) fn parse_timestamp(s: &str) -> Result<Timestamp, String> {
    Timestamp::parse_rfc3339(s).map_err(|e| {
        // TimeError's syntax messages start with their own "invalid
        // timestamp:" prefix; do not say it twice.
        let detail = e.to_string();
        let detail = detail
            .strip_prefix("invalid timestamp: ")
            .unwrap_or(&detail);
        format!("invalid timestamp {}: {detail}", quote(s))
    })
}

/// Encodes a duration in the compact form of
/// [`belay_common::format_duration`].
#[must_use]
pub fn duration_to_json(d: Duration) -> Value {
    Value::String(format_duration(d))
}

/// Decodes a duration string.
///
/// ```
/// use std::time::Duration;
/// use belay_json::json;
/// use belay_proto::wire::duration_from_json;
///
/// assert_eq!(duration_from_json(&json!("1m30s")), Ok(Duration::from_secs(90)));
/// let err = duration_from_json(&json!(30000)).unwrap_err();
/// assert_eq!(
///     err.message(),
///     r#"expected duration string such as "30s" or "1h30m", found 30000"#
/// );
/// ```
pub fn duration_from_json(value: &Value) -> Result<Duration, FromJsonError> {
    let s = value
        .as_str()
        .ok_or_else(|| FromJsonError::expected(DURATION_EXPECTATION, value))?;
    parse_duration(s).map_err(|e| FromJsonError::new(e.to_string()))
}
