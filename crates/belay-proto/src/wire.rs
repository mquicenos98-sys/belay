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

/// Encodes a backoff policy in its text form.
#[must_use]
pub fn backoff_to_json(policy: &BackoffPolicy) -> Value {
    Value::String(policy.to_string())
}

/// Decodes a backoff policy from its text form.
pub fn backoff_from_json(value: &Value) -> Result<BackoffPolicy, FromJsonError> {
    let s = value.as_str().ok_or_else(|| {
        FromJsonError::expected(r#"backoff string such as "exp:1s*2..10m""#, value)
    })?;
    s.parse::<BackoffPolicy>()
        .map_err(|e| FromJsonError::new(e.to_string()))
}

/// Decodes a JSON string through `T::from_str` for this crate's text types,
/// whose [`ParseError`] messages already name the type and echo the input.
pub(crate) fn text_from_json<T>(value: &Value, what: &str) -> Result<T, FromJsonError>
where
    T: FromStr<Err = ParseError>,
{
    let s = value
        .as_str()
        .ok_or_else(|| FromJsonError::expected(format_args!("{what} string"), value))?;
    s.parse()
        .map_err(|e: ParseError| FromJsonError::new(e.to_string()))
}

/// Implements `ToJson`/`FromJson` through `Display`/`FromStr` for a type
/// whose `FromStr::Err` is [`ParseError`].
macro_rules! text_json {
    ($ty:ty, $what:expr) => {
        impl belay_json::ToJson for $ty {
            fn to_json(&self) -> belay_json::Value {
                belay_json::Value::String(self.to_string())
            }
        }

        impl belay_json::FromJson for $ty {
            fn from_json(
                value: &belay_json::Value,
            ) -> ::std::result::Result<Self, belay_json::FromJsonError> {
                $crate::wire::text_from_json(value, $what)
            }
        }
    };
}
pub(crate) use text_json;

/// Declares a fieldless enum whose wire form is a fixed lowercase name per
/// variant, with `ALL`, `as_str`, `Display`, `FromStr` (with "did you mean"
/// hints) and JSON impls.
macro_rules! text_enum {
    (
        $(#[$meta:meta])*
        $name:ident, $what:literal {
            $($(#[$vmeta:meta])* $variant:ident = $text:literal,)+
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$vmeta])* $variant,)+
        }

        impl $name {
            /// Every variant, in declaration order.
            pub const ALL: &'static [$name] = &[$($name::$variant),+];
            const NAMES: &'static [&'static str] = &[$($text),+];

            /// The text form.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $text,)+
                }
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.pad(self.as_str())
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = $crate::error::ParseError;

            fn from_str(s: &str) -> ::std::result::Result<$name, $crate::error::ParseError> {
                $name::ALL
                    .iter()
                    .copied()
                    .find(|v| v.as_str() == s)
                    .ok_or_else(|| $crate::error::ParseError::unknown($what, s, $name::NAMES))
            }
        }

        $crate::wire::text_json!($name, $what);
    };
}
pub(crate) use text_enum;

/// Number of bytes `value` occupies as compact JSON, computed without
/// building the string.
///
/// ```
/// use belay_json::json;
/// use belay_proto::wire::encoded_len;
///
/// let v = json!({"to": "ada@example.com", "n": [1, 2]});
/// assert_eq!(encoded_len(&v), v.to_string().len());
/// ```
#[must_use]
pub fn encoded_len(value: &Value) -> usize {
    struct Counter(usize);

    impl io::Write for Counter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut counter = Counter(0);
    // The counter never fails, so neither does the writer.
    let _ = belay_json::write_to(&mut counter, value);
    counter.0
}

/// Typed field accessors for [`ObjectReader`] covering the belay-common
/// types.
pub trait ReaderExt {
    /// A required RFC 3339 timestamp.
    fn req_timestamp(&self, key: &'static str) -> Result<Timestamp, FromJsonError>;
    /// An optional RFC 3339 timestamp (absent or `null` is `None`).
    fn opt_timestamp(&self, key: &'static str) -> Result<Option<Timestamp>, FromJsonError>;
    /// A required duration string.
    fn req_duration(&self, key: &'static str) -> Result<Duration, FromJsonError>;
    /// An optional duration string.
    fn opt_duration(&self, key: &'static str) -> Result<Option<Duration>, FromJsonError>;
    /// A duration string, `default` when absent or `null`.
    fn duration_or(&self, key: &'static str, default: Duration) -> Result<Duration, FromJsonError>;
    /// An optional backoff policy string.
    fn opt_backoff(&self, key: &'static str) -> Result<Option<BackoffPolicy>, FromJsonError>;
}

impl ReaderExt for ObjectReader<'_> {
    fn req_timestamp(&self, key: &'static str) -> Result<Timestamp, FromJsonError> {
        self.req_with(key, timestamp_from_json)
    }

    fn opt_timestamp(&self, key: &'static str) -> Result<Option<Timestamp>, FromJsonError> {
        self.opt_with(key, timestamp_from_json)
    }

    fn req_duration(&self, key: &'static str) -> Result<Duration, FromJsonError> {
        self.req_with(key, duration_from_json)
    }

    fn opt_duration(&self, key: &'static str) -> Result<Option<Duration>, FromJsonError> {
        self.opt_with(key, duration_from_json)
    }

    fn duration_or(&self, key: &'static str, default: Duration) -> Result<Duration, FromJsonError> {
        Ok(self.opt_duration(key)?.unwrap_or(default))
    }

    fn opt_backoff(&self, key: &'static str) -> Result<Option<BackoffPolicy>, FromJsonError> {
        self.opt_with(key, backoff_from_json)
    }
}
