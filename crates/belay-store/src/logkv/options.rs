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

impl fmt::Display for ParseOptionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid {} {:?} (expected {})",
            self.what, self.input, self.expected
        )
    }
}

impl std::error::Error for ParseOptionError {}

impl FromStr for SyncPolicy {
    type Err = ParseOptionError;

    /// Parses `always`, `never` or `interval:<duration>` (e.g.
    /// `interval:10ms`), case-insensitively.
    fn from_str(s: &str) -> Result<SyncPolicy, ParseOptionError> {
        let err = || ParseOptionError {
            what: "sync policy",
            input: s.to_string(),
            expected: "always, never or interval:<duration> such as interval:10ms",
        };
        let lower = s.trim().to_ascii_lowercase();
        match lower.as_str() {
            "always" => Ok(SyncPolicy::Always),
            "never" => Ok(SyncPolicy::Never),
            other => {
                let d = other
                    .strip_prefix("interval:")
                    .and_then(|d| parse_duration(d).ok())
                    .ok_or_else(err)?;
                if d.is_zero() {
                    return Err(err());
                }
                Ok(SyncPolicy::Interval(d))
            }
        }
    }
}

/// How recovery treats damage that cannot be explained by a crash.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RecoveryMode {
    /// Refuse to open: report [`StoreError::Corrupt`] with the file and
    /// offset. A torn log tail is still truncated, since it is the normal
    /// result of a crash.
    #[default]
    Strict,
    /// Skip damaged records, log gaps and unreadable snapshots with a
    /// warning and recover everything else. After such a recovery the
    /// store writes a fresh checkpoint so the damage is not met again.
    SkipCorrupt,
}

impl fmt::Display for RecoveryMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RecoveryMode::Strict => "strict",
            RecoveryMode::SkipCorrupt => "skip-corrupt",
        })
    }
}
