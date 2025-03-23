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

impl Wal {
    fn new(active: SegmentWriter, sealed: BTreeMap<u64, u64>, written_seq: u64) -> Wal {
        let sealed_bytes = sealed.values().sum();
        Wal {
            active: Some(active),
            sealed,
            sealed_bytes,
            written_seq,
        }
    }

    fn seal(&mut self, first_seq: u64, size: u64) {
        if let Some(old) = self.sealed.insert(first_seq, size) {
            self.sealed_bytes -= old;
        }
        self.sealed_bytes += size;
    }

    fn forget(&mut self, first_seq: u64) {
        if let Some(size) = self.sealed.remove(&first_seq) {
            self.sealed_bytes -= size;
        }
    }

    fn live_bytes(&self) -> u64 {
        self.sealed_bytes
    }

    fn segment_count(&self) -> u64 {
        self.sealed.len() as u64 + u64::from(self.active.is_some())
    }
}

/// Monotonic counters for [`KvStats`].
#[derive(Debug, Default)]
struct Counters {
    batches: AtomicU64,
    ops: AtomicU64,
    groups: AtomicU64,
    syncs: AtomicU64,
    bytes: AtomicU64,
    checkpoints: AtomicU64,
}

impl Counters {
    fn add(&self, counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    fn get(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }
}

/// State shared between the handle and the background thread.
struct Inner {
    dir: PathBuf,
    fs: Arc<dyn Vfs>,
    sync: SyncPolicy,
    segment_size: u64,
    checkpoint_bytes: Option<u64>,

    visible: RwLock<Visible>,
    commit: Mutex<CommitState>,
    /// Signalled when a group finishes, durability advances or the store
    /// fails.
    committed: Condvar,
    wal: Mutex<Wal>,
    checkpoint_lock: Mutex<()>,
    /// Sequence number of the newest snapshot.
    checkpoint_seq: AtomicU64,
    /// Its size; 0 while there is none.
    snapshot_bytes: AtomicU64,
    bg: Mutex<Background>,
    bg_signal: Condvar,

    counters: Counters,
    recovery: RecoveryStats,
    closed: AtomicBool,
    dir_lock: Mutex<Option<Box<dyn VfsLock>>>,
}
