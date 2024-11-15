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
