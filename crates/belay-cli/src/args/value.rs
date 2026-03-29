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
