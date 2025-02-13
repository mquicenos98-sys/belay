//! The store's error type.
//!
//! I/O failures always carry the operation and the file involved, because
//! "permission denied" is useless without knowing which of a few hundred
//! segment files it was about. Corruption carries the byte offset so an
//! operator can look at the damage with `belay admin store-inspect`.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use belay_common::fs::LockError;

/// Any error returned by this crate.
#[derive(Debug)]
#[non_exhaustive]
pub enum StoreError {
    /// A file operation failed.
    Io {
        /// What was being done, e.g. `"syncing"`.
        op: &'static str,
        /// The file or directory involved.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// A file failed validation: bad magic, checksum mismatch, a gap in the
    /// log, and so on.
    Corrupt {
        /// The damaged file.
        file: PathBuf,
        /// Byte offset of the damage within `file`.
        offset: u64,
        /// What is wrong.
        reason: String,
    },
    /// Another process (or another handle in this one) has the store open.
    Locked {
        /// The lock file.
        path: PathBuf,
        /// The holder's process id, when known.
        pid: Option<u32>,
    },
    /// The store was closed.
    Closed,
    /// A write-ahead log write or sync failed earlier, so the store no
    /// longer accepts writes. Reopening it recovers from the log.
    Failed {
        /// The original failure.
        reason: String,
    },
    /// A key, value or batch exceeds a size limit.
    TooLarge {
        /// `"key"`, `"value"` or `"batch"`.
        what: &'static str,
        /// The offending size in bytes.
        len: usize,
        /// The limit in bytes.
        max: usize,
    },
    /// [`Options`](crate::Options) are inconsistent or out of range.
    InvalidOptions(String),
    /// The store directory does not exist and creating it was not allowed.
    NotFound {
        /// The missing directory.
        dir: PathBuf,
    },
}

/// Conventional alias, so downstream code can write `belay_store::Error`.
pub type Error = StoreError;

/// `Result` defaulting to [`StoreError`].
pub type Result<T, E = StoreError> = std::result::Result<T, E>;

impl StoreError {
    /// An [`Io`](StoreError::Io) error.
    pub fn io(op: &'static str, path: impl Into<PathBuf>, source: io::Error) -> StoreError {
        StoreError::Io {
            op,
            path: path.into(),
            source,
        }
    }

    /// A [`Corrupt`](StoreError::Corrupt) error.
    pub fn corrupt(file: impl Into<PathBuf>, offset: u64, reason: impl Into<String>) -> StoreError {
        StoreError::Corrupt {
            file: file.into(),
            offset,
            reason: reason.into(),
        }
    }

    /// Whether this error reports damaged files rather than an operational
    /// problem; callers may offer `RecoveryMode::SkipCorrupt` in response.
    #[must_use]
    pub fn is_corruption(&self) -> bool {
        matches!(self, StoreError::Corrupt { .. })
    }

    /// The file this error is about, if any.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        match self {
            StoreError::Io { path, .. } | StoreError::Locked { path, .. } => Some(path),
            StoreError::Corrupt { file, .. } => Some(file),
            StoreError::NotFound { dir } => Some(dir),
            _ => None,
        }
    }
}

/// Builds a closure that wraps an `io::Error` with an operation and path,
/// for use with `map_err`.
pub(crate) fn io_err<'a>(
    op: &'static str,
    path: &'a Path,
) -> impl FnOnce(io::Error) -> StoreError + 'a {
    move |source| StoreError::io(op, path, source)
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Io { op, path, source } => {
                write!(f, "{op} {}: {source}", path.display())
            }
            StoreError::Corrupt {
                file,
                offset,
                reason,
            } => write!(f, "corrupt {} at offset {offset}: {reason}", file.display()),
            StoreError::Locked {
                path,
                pid: Some(pid),
            } => write!(
                f,
                "store is locked by process {pid} ({}); if that process is gone, delete the file",
                path.display()
            ),
            StoreError::Locked { path, pid: None } => write!(
                f,
                "store is locked by another process or handle ({})",
                path.display()
            ),
            StoreError::Closed => f.write_str("store is closed"),
            StoreError::Failed { reason } => write!(
                f,
                "store stopped accepting writes after a failure ({reason}); reopen it to recover"
            ),
            StoreError::TooLarge { what, len, max } => {
                write!(f, "{what} is {len} bytes, limit is {max}")
            }
            StoreError::InvalidOptions(message) => write!(f, "invalid store options: {message}"),
            StoreError::NotFound { dir } => {
                write!(f, "store directory {} does not exist", dir.display())
            }
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<LockError> for StoreError {
    fn from(e: LockError) -> StoreError {
        match e {
            LockError::Held { path, pid } => StoreError::Locked { path, pid },
            LockError::Io { path, source } => StoreError::io("locking", path, source),
            // `LockError` is non-exhaustive; keep whatever it says.
            other => StoreError::Io {
                op: "locking",
                path: PathBuf::new(),
                source: io::Error::new(io::ErrorKind::Other, other.to_string()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn io_errors_name_the_operation_and_file() {
        let e = StoreError::io(
            "syncing",
            "/data/wal-00000000000000000001.log",
            io::Error::new(io::ErrorKind::Other, "disk on fire"),
        );
        assert_eq!(
            e.to_string(),
            "syncing /data/wal-00000000000000000001.log: disk on fire"
        );
        assert!(e.source().is_some());
        assert_eq!(
            e.path(),
            Some(Path::new("/data/wal-00000000000000000001.log"))
        );
        assert!(!e.is_corruption());
    }

    #[test]
    fn corruption_reports_offset_and_reason() {
        let e = StoreError::corrupt("snap.db", 4096, "checksum mismatch");
        assert_eq!(
            e.to_string(),
            "corrupt snap.db at offset 4096: checksum mismatch"
        );
        assert!(e.is_corruption());
        assert!(e.source().is_none());
    }

    #[test]
    fn size_and_state_errors_read_naturally() {
        let e = StoreError::TooLarge {
            what: "key",
            len: 70_000,
            max: 65_536,
        };
        assert_eq!(e.to_string(), "key is 70000 bytes, limit is 65536");
        assert_eq!(StoreError::Closed.to_string(), "store is closed");
        let e = StoreError::Failed {
            reason: "no space left".into(),
        };
        assert!(e.to_string().contains("reopen it to recover"), "{e}");
        assert_eq!(e.path(), None);
    }

    #[test]
    fn lock_errors_convert() {
        let e = StoreError::from(LockError::Held {
            path: PathBuf::from("LOCK"),
            pid: Some(42),
        });
        assert!(matches!(e, StoreError::Locked { pid: Some(42), .. }));
        assert!(e.to_string().contains("process 42"), "{e}");
        let e = StoreError::from(LockError::Io {
            path: PathBuf::from("LOCK"),
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        });
        assert!(e.to_string().starts_with("locking LOCK: "), "{e}");
    }
}
