//! Typed flag and argument values: the [`FromArg`] trait and its
//! implementations for numbers, durations, timestamps and belay's
//! identifiers and policies.

use std::num::IntErrorKind;
use std::path::PathBuf;
use std::time::Duration;

use belay_common::{parse_duration, BackoffPolicy, Timestamp};
use belay_proto::api::SortOrder;
use belay_proto::event::EventPattern;
use belay_proto::{
    JobId, JobState, MisfirePolicy, OnConflict, OverlapPolicy, QueueName, ScheduleId, TenantId,
    UniqueScope, WorkerId, WorkflowId,
};

/// A type that can be read from one command-line value.
///
/// The error is the *reason* only (`expected an integer`); the caller adds
/// which flag and value it concerns.
pub trait FromArg: Sized {
    /// Parses `text`.
    fn from_arg(text: &str) -> Result<Self, String>;
}

macro_rules! integer_args {
    ($($t:ty),*) => {$(
        impl FromArg for $t {
            fn from_arg(text: &str) -> Result<$t, String> {
                text.parse::<$t>().map_err(|e| match e.kind() {
                    IntErrorKind::PosOverflow | IntErrorKind::NegOverflow => format!(
                        "expected an integer between {} and {}",
                        <$t>::MIN,
                        <$t>::MAX
                    ),
                    _ if <$t>::MIN == 0 && text.starts_with('-') => {
                        "expected a non-negative integer".to_owned()
                    }
                    _ => "expected an integer".to_owned(),
                })
            }
        }
    )*};
}

integer_args!(u8, u16, u32, u64, usize, i16, i32, i64);

impl FromArg for f64 {
    fn from_arg(text: &str) -> Result<f64, String> {
        match text.parse::<f64>() {
            Ok(v) if v.is_finite() => Ok(v),
            _ => Err("expected a number".to_owned()),
        }
    }
}

impl FromArg for String {
    fn from_arg(text: &str) -> Result<String, String> {
        Ok(text.to_owned())
    }
}

impl FromArg for PathBuf {
    fn from_arg(text: &str) -> Result<PathBuf, String> {
        if text.is_empty() {
            Err("expected a path".to_owned())
        } else {
            Ok(PathBuf::from(text))
        }
    }
}

impl FromArg for Duration {
    fn from_arg(text: &str) -> Result<Duration, String> {
        parse_duration(text).map_err(|e| {
            format!(
                "{} (write durations like 30s, 1h30m or 250ms)",
                reason(&e.to_string(), text)
            )
        })
    }
}

impl FromArg for Timestamp {
    fn from_arg(text: &str) -> Result<Timestamp, String> {
        Timestamp::parse_rfc3339(text).map_err(|_| {
            "expected an RFC 3339 time such as 2025-03-04T09:30:00Z or \
             2025-03-04T09:30:00-05:00"
                .to_owned()
        })
    }
}

/// Reads through `FromStr`, keeping just the reason from messages shaped
/// `invalid <what> "<input>": <reason>` so the caller's prefix does not
/// repeat the input.
macro_rules! from_str_args {
    ($($t:ty),*) => {$(
        impl FromArg for $t {
            fn from_arg(text: &str) -> Result<$t, String> {
                text.parse::<$t>().map_err(|e| reason(&e.to_string(), text))
            }
        }
    )*};
}

from_str_args!(
    QueueName,
    JobId,
    JobState,
    WorkerId,
    WorkflowId,
    ScheduleId,
    TenantId,
    MisfirePolicy,
    OverlapPolicy,
    OnConflict,
    UniqueScope,
    SortOrder,
    BackoffPolicy,
    EventPattern
);

/// The part of an error message after the echoed input, if the message
/// echoes it; otherwise the whole message.
fn reason(message: &str, input: &str) -> String {
    for quoted in [format!("\"{input}\": "), format!("{input:?}: ")] {
        if let Some(at) = message.find(&quoted) {
            return message[at + quoted.len()..].to_owned();
        }
    }
    message.to_owned()
}

/// A `KEY=VALUE` pair, as taken by `--meta` and `--set`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyValue {
    /// Text before the first `=`; never empty.
    pub key: String,
    /// Text after it; may be empty.
    pub value: String,
}
