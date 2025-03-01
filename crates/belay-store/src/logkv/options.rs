//! Configuration of a [`LogKv`](super::LogKv).

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use belay_common::{format_duration, parse_duration};

use crate::error::{Result, StoreError};
use crate::vfs::{OsFs, Vfs};

/// When the write-ahead log is synced to stable storage.
///
/// Whatever the policy, recovery restores the state after some *prefix*
/// of the acknowledged batches, never a state with a batch missing in the
/// middle, and a process crash that leaves the operating system running
/// loses nothing that was acknowledged. The policies differ in what a
/// power loss or kernel crash can take away:
///
/// | policy | `write` returns after | lost on power loss |
/// |---|---|---|
/// | `Always` | the batch is synced | nothing acknowledged |
/// | `Interval(d)` | the batch is handed to the OS | at most the batches acknowledged in the last `d` plus one sync |
/// | `Never` | the batch is handed to the OS | anything since the last segment roll, checkpoint, `flush` or `close` |
///
/// Under `Always`, concurrent writers share syncs (group commit), so the
/// cost is one sync per group rather than per batch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SyncPolicy {
    /// Sync before acknowledging each write.
    #[default]
    Always,
    /// Sync from a background thread at this interval.
    Interval(Duration),
    /// Sync only when a segment fills up, on checkpoints, on
    /// [`flush`](crate::Kv::flush) and on close.
    Never,
}

impl fmt::Display for SyncPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyncPolicy::Always => f.write_str("always"),
            SyncPolicy::Interval(d) => write!(f, "interval:{}", format_duration(*d)),
            SyncPolicy::Never => f.write_str("never"),
        }
    }
}

/// Error parsing a [`SyncPolicy`] or [`RecoveryMode`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseOptionError {
    what: &'static str,
    input: String,
    expected: &'static str,
}
