//! The durable store: an in-memory table backed by a write-ahead log and
//! periodic snapshots.
//!
//! The whole dataset lives in a `BTreeMap`. That is a deliberate choice:
//! belay stores job metadata, queue configuration and schedules, which are
//! small, and payloads are expected to be modest, so memory is not the
//! constraint and reads never touch the disk. Durability comes from the
//! log (see the `wal` module for its format and the torn-write rules) and
//! from snapshots written by checkpoints (see `snapshot`), which also bound
//! the log's size and the time recovery takes.
//!
//! # Locking
//!
//! Lock order is `checkpoint_lock`, then `wal`, then `visible`. The
//! commit-queue mutex and the background-thread mutex come last: they may
//! be taken while holding the others, but nothing else is acquired while
//! holding them. Readers take only `visible` (a read lock), so reads
//! proceed while the log is being written and synced.

mod checkpoint;

mod commit;

mod options;

mod recovery;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::{self, JoinHandle};

use belay_common::sync::{lock, read, wait};
use belay_common::{debug, info, warn};

use self::checkpoint::{run_background, Background};
use self::commit::CommitState;
use crate::batch::WriteBatch;
use crate::error::{io_err, Result, StoreError};
use crate::files::LOCK_NAME;
use crate::kv::{CheckpointInfo, Kv, KvStats, RecoveryStats};
use crate::range::KeyRange;
use crate::table::Table;
use crate::vfs::{Vfs, VfsLock};
use crate::wal::SegmentWriter;

pub use options::{Options, ParseOptionError, RecoveryMode, SyncPolicy};

/// The table as readers see it.
struct Visible {
    table: Table,
    /// Sequence number of the last batch applied.
    seq: u64,
}

/// The log files, guarded by the `wal` lock.
struct Wal {
    /// Where batches are appended; `None` once closed.
    active: Option<SegmentWriter>,
    /// Older segments still on disk: first sequence number to size.
    sealed: BTreeMap<u64, u64>,
    /// Sum of the sizes in `sealed`, kept so that the per-commit
    /// checkpoint check does not walk the map.
    sealed_bytes: u64,
    /// Sequence number of the last record appended.
    written_seq: u64,
}
