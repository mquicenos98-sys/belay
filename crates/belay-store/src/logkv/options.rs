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

impl FromStr for RecoveryMode {
    type Err = ParseOptionError;

    fn from_str(s: &str) -> Result<RecoveryMode, ParseOptionError> {
        match s.trim().to_ascii_lowercase().as_str() {
            "strict" => Ok(RecoveryMode::Strict),
            "skip-corrupt" | "skip_corrupt" => Ok(RecoveryMode::SkipCorrupt),
            _ => Err(ParseOptionError {
                what: "recovery mode",
                input: s.to_string(),
                expected: "strict or skip-corrupt",
            }),
        }
    }
}

/// Settings for [`LogKv::open`](super::LogKv::open), built with chained
/// methods.
///
/// ```
/// use belay_store::vfs::MemFs;
/// use belay_store::{Options, SyncPolicy};
/// use std::time::Duration;
///
/// let opts = Options::new()
///     .fs(MemFs::new())
///     .sync(SyncPolicy::Interval(Duration::from_millis(10)))
///     .segment_size(8 << 20)
///     .checkpoint_every(64 << 20);
/// assert_eq!(opts.sync_policy().to_string(), "interval:10ms");
/// ```
#[derive(Clone)]
pub struct Options {
    pub(crate) fs: Arc<dyn Vfs>,
    pub(crate) sync: SyncPolicy,
    pub(crate) segment_size: u64,
    pub(crate) checkpoint_bytes: Option<u64>,
    pub(crate) recovery: RecoveryMode,
    pub(crate) create_if_missing: bool,
}

impl Options {
    /// Default size at which the active log segment is sealed and a new
    /// one started.
    pub const DEFAULT_SEGMENT_SIZE: u64 = 32 * 1024 * 1024;
    /// Default amount of log that triggers an automatic checkpoint.
    pub const DEFAULT_CHECKPOINT_BYTES: u64 = 128 * 1024 * 1024;
    /// Smallest accepted segment size.
    pub const MIN_SEGMENT_SIZE: u64 = 128;
    /// Largest accepted segment size.
    pub const MAX_SEGMENT_SIZE: u64 = 1 << 30;

    /// Defaults: the real file system, [`SyncPolicy::Always`], 32 MiB
    /// segments, a checkpoint every 128 MiB of log, strict recovery, and
    /// creating the directory if needed.
    #[must_use]
    pub fn new() -> Options {
        Options {
            fs: Arc::new(OsFs),
            sync: SyncPolicy::Always,
            segment_size: Options::DEFAULT_SEGMENT_SIZE,
            checkpoint_bytes: Some(Options::DEFAULT_CHECKPOINT_BYTES),
            recovery: RecoveryMode::Strict,
            create_if_missing: true,
        }
    }

    /// Uses `fs` for all file access, e.g. a [`MemFs`](crate::vfs::MemFs)
    /// in tests.
    #[must_use]
    pub fn fs(self, fs: impl Vfs + 'static) -> Options {
        self.shared_fs(Arc::new(fs))
    }

    /// Like [`fs`](Self::fs) for an already shared file system.
    #[must_use]
    pub fn shared_fs(mut self, fs: Arc<dyn Vfs>) -> Options {
        self.fs = fs;
        self
    }

    /// Sets the sync policy.
    #[must_use]
    pub fn sync(mut self, policy: SyncPolicy) -> Options {
        self.sync = policy;
        self
    }

    /// Seals the active log segment and starts a new one once it holds at
    /// least this many bytes. A single large batch may overshoot it.
    #[must_use]
    pub fn segment_size(mut self, bytes: u64) -> Options {
        self.segment_size = bytes;
        self
    }

    /// Checkpoints automatically, on a background thread, once the log
    /// segments on disk add up to `bytes`. Keep it well above the segment
    /// size: log is only discarded a whole segment at a time.
    #[must_use]
    pub fn checkpoint_every(mut self, bytes: u64) -> Options {
        self.checkpoint_bytes = Some(bytes);
        self
    }

    /// Disables automatic checkpoints; only [`Kv::checkpoint`](crate::Kv::checkpoint)
    /// writes them.
    #[must_use]
    pub fn manual_checkpoints(mut self) -> Options {
        self.checkpoint_bytes = None;
        self
    }

    /// Sets how recovery treats corruption.
    #[must_use]
    pub fn recovery(mut self, mode: RecoveryMode) -> Options {
        self.recovery = mode;
        self
    }

    /// Whether opening a missing directory creates it (the default) or
    /// fails with [`StoreError::NotFound`].
    #[must_use]
    pub fn create_if_missing(mut self, create: bool) -> Options {
        self.create_if_missing = create;
        self
    }

    /// The sync policy.
    #[must_use]
    pub fn sync_policy(&self) -> SyncPolicy {
        self.sync
    }

    /// The recovery mode.
    #[must_use]
    pub fn recovery_mode(&self) -> RecoveryMode {
        self.recovery
    }

    /// The segment size.
    #[must_use]
    pub fn segment_bytes(&self) -> u64 {
        self.segment_size
    }

    /// The automatic checkpoint threshold, if enabled.
    #[must_use]
    pub fn checkpoint_bytes(&self) -> Option<u64> {
        self.checkpoint_bytes
    }

    pub(crate) fn validate(&self) -> Result<()> {
        let invalid = |msg: String| Err(StoreError::InvalidOptions(msg));
        if !(Options::MIN_SEGMENT_SIZE..=Options::MAX_SEGMENT_SIZE).contains(&self.segment_size) {
            return invalid(format!(
                "segment size must be between {} and {} bytes, got {}",
                Options::MIN_SEGMENT_SIZE,
                Options::MAX_SEGMENT_SIZE,
                self.segment_size
            ));
        }
        if self.checkpoint_bytes == Some(0) {
            return invalid(
                "checkpoint threshold must be positive (use manual checkpoints to disable)".into(),
            );
        }
        if self.sync == SyncPolicy::Interval(Duration::ZERO) {
            return invalid("sync interval must be positive".into());
        }
        Ok(())
    }
}

impl Default for Options {
    fn default() -> Options {
        Options::new()
    }
}

impl fmt::Debug for Options {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Options")
            .field("fs", &self.fs)
            .field("sync", &self.sync)
            .field("segment_size", &self.segment_size)
            .field("checkpoint_bytes", &self.checkpoint_bytes)
            .field("recovery", &self.recovery)
            .field("create_if_missing", &self.create_if_missing)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_policies_parse_and_display() {
        assert_eq!("always".parse(), Ok(SyncPolicy::Always));
        assert_eq!(" NEVER ".parse(), Ok(SyncPolicy::Never));
        assert_eq!(
            "interval:10ms".parse(),
            Ok(SyncPolicy::Interval(Duration::from_millis(10)))
        );
        for policy in [
            SyncPolicy::Always,
            SyncPolicy::Never,
            SyncPolicy::Interval(Duration::from_millis(1500)),
        ] {
            assert_eq!(policy.to_string().parse(), Ok(policy));
        }
        for bad in [
            "sometimes",
            "interval:",
            "interval:0s",
            "interval:5x",
            "10ms",
        ] {
            let e = bad.parse::<SyncPolicy>().unwrap_err();
            assert!(e.to_string().starts_with("invalid sync policy"), "{e}");
        }
    }

    #[test]
    fn recovery_modes_parse_and_display() {
        assert_eq!("strict".parse(), Ok(RecoveryMode::Strict));
        assert_eq!("Skip-Corrupt".parse(), Ok(RecoveryMode::SkipCorrupt));
        assert_eq!(RecoveryMode::SkipCorrupt.to_string(), "skip-corrupt");
        assert_eq!(
            "lenient".parse::<RecoveryMode>().unwrap_err().to_string(),
            "invalid recovery mode \"lenient\" (expected strict or skip-corrupt)"
        );
    }

    #[test]
    fn validation_rejects_out_of_range_settings() {
        Options::new().validate().expect("defaults are valid");
        let e = Options::new().segment_size(10).validate().unwrap_err();
        assert!(
            e.to_string().contains("segment size must be between 128"),
            "{e}"
        );
        assert!(Options::new().segment_size(1 << 31).validate().is_err());
        assert!(Options::new().checkpoint_every(0).validate().is_err());
        assert!(Options::new()
            .sync(SyncPolicy::Interval(Duration::ZERO))
            .validate()
            .is_err());
        Options::new()
            .manual_checkpoints()
            .validate()
            .expect("manual checkpoints are valid");
    }

    #[test]
    fn builder_sets_every_field() {
        let opts = Options::default()
            .sync(SyncPolicy::Never)
            .segment_size(4096)
            .manual_checkpoints()
            .recovery(RecoveryMode::SkipCorrupt)
            .create_if_missing(false);
        assert_eq!(opts.sync_policy(), SyncPolicy::Never);
        assert_eq!(opts.segment_bytes(), 4096);
        assert_eq!(opts.checkpoint_bytes(), None);
        assert_eq!(opts.recovery_mode(), RecoveryMode::SkipCorrupt);
        assert!(!opts.create_if_missing);
        assert!(format!("{opts:?}").contains("OsFs"));
    }
}
