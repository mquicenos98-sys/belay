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

/// Field writers for [`ObjectBuilder`] covering the belay-common types.
/// `opt_*` variants omit the field when the value is `None`.
pub trait BuilderExt: Sized {
    /// Writes an RFC 3339 timestamp.
    fn timestamp(self, key: &str, ts: Timestamp) -> Self;
    /// Writes a timestamp if present.
    fn opt_timestamp(self, key: &str, ts: Option<Timestamp>) -> Self;
    /// Writes a compact duration.
    fn duration(self, key: &str, d: Duration) -> Self;
    /// Writes a duration if present.
    fn opt_duration(self, key: &str, d: Option<Duration>) -> Self;
    /// Writes a backoff policy if present.
    fn opt_backoff(self, key: &str, policy: Option<&BackoffPolicy>) -> Self;
}

impl BuilderExt for ObjectBuilder {
    fn timestamp(self, key: &str, ts: Timestamp) -> Self {
        self.raw(key, timestamp_to_json(ts))
    }

    fn opt_timestamp(self, key: &str, ts: Option<Timestamp>) -> Self {
        match ts {
            Some(ts) => self.timestamp(key, ts),
            None => self,
        }
    }

    fn duration(self, key: &str, d: Duration) -> Self {
        self.raw(key, duration_to_json(d))
    }

    fn opt_duration(self, key: &str, d: Option<Duration>) -> Self {
        match d {
            Some(d) => self.duration(key, d),
            None => self,
        }
    }

    fn opt_backoff(self, key: &str, policy: Option<&BackoffPolicy>) -> Self {
        match policy {
            Some(p) => self.raw(key, backoff_to_json(p)),
            None => self,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_json::json;

    #[test]
    fn timestamps_normalise_offsets_and_reject_numbers() {
        let t = timestamp_from_json(&json!("2024-02-29T23:30:00-01:30")).unwrap();
        assert_eq!(timestamp_to_json(t), json!("2024-03-01T01:00:00.000Z"));
        let err = timestamp_from_json(&json!("2023-02-29T00:00:00Z")).unwrap_err();
        assert_eq!(
            err.message(),
            r#"invalid timestamp "2023-02-29T00:00:00Z": day 29 is out of range (1-28)"#
        );
        let err = timestamp_from_json(&json!("yesterday")).unwrap_err();
        assert_eq!(
            err.message(),
            r#"invalid timestamp "yesterday": expected 4-digit year at byte 0"#
        );
    }

    #[test]
    fn durations_round_trip_and_explain_numbers() {
        for d in [
            Duration::ZERO,
            Duration::from_millis(250),
            Duration::from_secs(5_400),
            Duration::from_micros(1_500),
        ] {
            assert_eq!(duration_from_json(&duration_to_json(d)), Ok(d));
        }
        assert_eq!(duration_to_json(Duration::from_secs(5_400)), json!("1h30m"));
        // A unitless string is seconds, as everywhere else in belay.
        assert_eq!(
            duration_from_json(&json!("90")),
            Ok(Duration::from_secs(90))
        );
        let err = duration_from_json(&json!("-5s")).unwrap_err();
        assert_eq!(
            err.message(),
            r#"invalid duration "-5s": negative durations are not allowed"#
        );
        assert!(duration_from_json(&json!(1.5)).is_err());
    }

    #[test]
    fn backoff_uses_the_text_form() {
        let p = backoff_from_json(&json!("exp:1s*2..10m~full")).unwrap();
        assert_eq!(backoff_to_json(&p), json!("exp:1s*2..10m~full"));
        let err = backoff_from_json(&json!("fib:1s")).unwrap_err();
        assert!(err.message().starts_with(r#"invalid backoff "fib:1s""#));
        let err = backoff_from_json(&json!({"kind": "exp"})).unwrap_err();
        assert_eq!(
            err.message(),
            r#"expected backoff string such as "exp:1s*2..10m", found object"#
        );
    }

    #[test]
    fn reader_and_builder_extensions_agree() {
        let t = Timestamp::from_millis(1_700_000_000_123);
        let v = ObjectBuilder::new()
            .timestamp("at", t)
            .opt_timestamp("never", None)
            .duration("d", Duration::from_secs(30))
            .opt_duration("none", None)
            .opt_backoff(
                "retry",
                Some(&BackoffPolicy::constant(Duration::from_secs(5))),
            )
            .build();
        assert_eq!(
            v,
            json!({"at": "2023-11-14T22:13:20.123Z", "d": "30s", "retry": "const:5s"})
        );
        let r = ObjectReader::new(&v, "T").unwrap();
        assert_eq!(r.req_timestamp("at"), Ok(t));
        assert_eq!(r.opt_timestamp("never"), Ok(None));
        assert_eq!(r.req_duration("d"), Ok(Duration::from_secs(30)));
        assert_eq!(
            r.duration_or("none", Duration::from_secs(1)),
            Ok(Duration::from_secs(1))
        );
        assert!(r.opt_backoff("retry").unwrap().is_some());
        let err = r.req_timestamp("d").unwrap_err();
        assert_eq!(
            err.to_string(),
            r#"$.d: invalid timestamp "30s": expected 4-digit year at byte 2"#
        );
    }

    #[test]
    fn encoded_len_matches_the_serializer() {
        let v = json!({"s": "é\u{1}\"", "n": [1.5, -3, null], "o": {}});
        assert_eq!(encoded_len(&v), belay_json::to_string(&v).len());
    }
}
