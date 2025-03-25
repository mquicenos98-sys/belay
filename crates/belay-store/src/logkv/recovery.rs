//! Rebuilding the state from the newest valid snapshot and the log,
//! following the procedure in `crate::format` ("Recovery").
//!
//! Nothing is modified until the whole log has been read, except for the
//! truncation of a torn tail, which is always the last step of reading.
//! A strict recovery that fails leaves the directory unchanged apart from
//! that truncation, for `belay admin verify` or a retry with
//! `RecoveryMode::SkipCorrupt`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use belay_common::{info, warn};

use super::options::RecoveryMode;
use crate::batch::WriteBatch;
use crate::error::{io_err, Result, StoreError};
use crate::files;
use crate::kv::RecoveryStats;
use crate::snapshot;
use crate::table::Table;
use crate::vfs::Vfs;
use crate::wal::{parse_segment_header, Event, Record, RecordReader, SEGMENT_HEADER_LEN};

/// The outcome of a successful recovery.
#[derive(Debug)]
pub(super) struct Recovered {
    pub(super) table: Table,
    /// Sequence number of the last batch restored.
    pub(super) last_seq: u64,
    /// Sequence number and size of the snapshot recovery started from.
    pub(super) snapshot_seq: u64,
    pub(super) snapshot_bytes: u64,
    /// Log segments left on disk, by first sequence number, with sizes.
    pub(super) segments: BTreeMap<u64, u64>,
    pub(super) stats: RecoveryStats,
}
