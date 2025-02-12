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
