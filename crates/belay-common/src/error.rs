//! The crate-wide error type.
//!
//! Each module returns its own specific error (`TimeError`, `DecodeError`,
//! ...), which is what callers usually match on. [`Error`] wraps all of them
//! for code that mixes several of these operations and just wants `?`.

use std::fmt;
use std::io;

use crate::backoff::ParseBackoffError;
use crate::codec::DecodeError;
use crate::duration::ParseDurationError;
use crate::fs::LockError;
use crate::id::{ParseUlidError, UlidOverflow};
use crate::log::LogConfigError;
use crate::text::ParseBytesError;
use crate::time::TimeError;

/// Any error produced by this crate.
///
/// ```
/// use belay_common::{parse_duration, Result, Timestamp};
///
/// fn deadline(start: &str, timeout: &str) -> Result<Timestamp> {
///     Ok(start.parse::<Timestamp>()? + parse_duration(timeout)?)
/// }
///
/// assert_eq!(
///     deadline("2025-01-01T00:00:00Z", "90s").unwrap().to_string(),
///     "2025-01-01T00:01:30.000Z"
/// );
/// assert_eq!(
///     deadline("2025-01-01T00:00:00Z", "5mins").unwrap_err().to_string(),
///     "invalid duration \"5mins\": unknown unit \"mins\" (expected ns, us, ms, s, m, h, d or w)"
/// );
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// A timestamp failed to parse or was out of range.
    Time(TimeError),
    /// A duration failed to parse.
    Duration(ParseDurationError),
    /// A ULID failed to parse.
    Ulid(ParseUlidError),
    /// A millisecond's ULID space ran out.
    UlidOverflow(UlidOverflow),
    /// A backoff policy failed to parse.
    Backoff(ParseBackoffError),
    /// Binary data failed to decode.
    Decode(DecodeError),
    /// A log level, filter or format failed to parse.
    LogConfig(LogConfigError),
    /// A byte size failed to parse.
    Bytes(ParseBytesError),
    /// A lock file could not be taken.
    Lock(LockError),
    /// An I/O operation failed.
    Io(io::Error),
}

/// `Result` defaulting to this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Time(e) => e.fmt(f),
            Error::Duration(e) => e.fmt(f),
            Error::Ulid(e) => e.fmt(f),
            Error::UlidOverflow(e) => e.fmt(f),
            Error::Backoff(e) => e.fmt(f),
            Error::Decode(e) => e.fmt(f),
            Error::LogConfig(e) => e.fmt(f),
            Error::Bytes(e) => e.fmt(f),
            Error::Lock(e) => e.fmt(f),
            Error::Io(e) => e.fmt(f),
        }
    }
}

/// Transparent: `Display` is the wrapped error's message, so `source` skips
/// the wrapped error (which would repeat that message) and returns its
/// source instead.
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Time(e) => e.source(),
            Error::Duration(e) => e.source(),
            Error::Ulid(e) => e.source(),
            Error::UlidOverflow(e) => e.source(),
            Error::Backoff(e) => e.source(),
            Error::Decode(e) => e.source(),
            Error::LogConfig(e) => e.source(),
            Error::Bytes(e) => e.source(),
            Error::Lock(e) => e.source(),
            Error::Io(e) => e.source(),
        }
    }
}

macro_rules! from_error {
    ($($variant:ident($ty:ty)),* $(,)?) => {$(
        impl From<$ty> for Error {
            fn from(e: $ty) -> Self {
                Error::$variant(e)
            }
        }
    )*};
}

from_error!(
    Time(TimeError),
    Duration(ParseDurationError),
    Ulid(ParseUlidError),
    UlidOverflow(UlidOverflow),
    Backoff(ParseBackoffError),
    Decode(DecodeError),
    LogConfig(LogConfigError),
    Bytes(ParseBytesError),
    Lock(LockError),
    Io(io::Error),
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Decoder;
    use std::error::Error as _;

    fn decode_two_bytes(bytes: &[u8]) -> Result<u16> {
        let mut dec = Decoder::new(bytes);
        let v = dec.get_u16()?;
        dec.expect_end()?;
        Ok(v)
    }

    #[test]
    fn question_mark_converts_module_errors() {
        assert_eq!(decode_two_bytes(&[1, 0]).ok(), Some(1));
        let err = decode_two_bytes(&[1]).unwrap_err();
        assert!(matches!(err, Error::Decode(_)));
        assert_eq!(
            err.to_string(),
            "decode error at offset 0: unexpected end of input (needed 2 bytes, 1 available)"
        );
        assert!(err.source().is_none());
    }

    #[test]
    fn source_skips_the_transparent_wrapper() {
        let err: Error = "exp:1q*2"
            .parse::<crate::backoff::BackoffPolicy>()
            .unwrap_err()
            .into();
        let source = err.source().expect("backoff errors wrap duration errors");
        assert_eq!(
            source.to_string(),
            "invalid duration \"1q\": unknown unit \"q\" (expected ns, us, ms, s, m, h, d or w)"
        );
    }

    #[test]
    fn display_delegates_to_the_wrapped_error() {
        let err: Error = "x".parse::<crate::id::Ulid>().unwrap_err().into();
        assert_eq!(
            err.to_string(),
            "invalid ULID: expected 26 characters, got 1"
        );
        let err: Error = io::Error::new(io::ErrorKind::NotFound, "gone").into();
        assert_eq!(err.to_string(), "gone");
        let err: Error = "loud".parse::<crate::log::Level>().unwrap_err().into();
        assert!(err.to_string().starts_with("unknown log level"));
    }
}
