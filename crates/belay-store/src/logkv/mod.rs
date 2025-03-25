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

impl Inner {
    fn ensure_open(&self) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            Err(StoreError::Closed)
        } else {
            Ok(())
        }
    }
}

/// A durable [`Kv`] backed by a directory of log segments and snapshots.
///
/// Reads are served from memory. Writes are appended to the log with group
/// commit and become visible when [`write`](Kv::write) returns; see
/// [`SyncPolicy`] for when they are durable. [`checkpoint`](Kv::checkpoint)
/// (or the automatic one, see [`Options::checkpoint_every`]) snapshots the
/// state and deletes the log it covers.
///
/// Opening recovers from the newest valid snapshot plus the log after it.
/// A torn log tail left by a crash is truncated with a warning and
/// counted in [`RecoveryStats`]; other damage is reported as
/// [`StoreError::Corrupt`] unless [`RecoveryMode::SkipCorrupt`] is set.
/// A `LOCK` file keeps a second process (or handle) from opening the same
/// directory.
///
/// ```
/// use belay_store::vfs::MemFs;
/// use belay_store::{KeyRange, Kv, LogKv, Options, WriteBatch};
///
/// let fs = MemFs::new();
/// let kv = LogKv::open("data", Options::new().fs(fs.clone())).unwrap();
/// let mut batch = WriteBatch::new();
/// batch.put("job/1", "ready").put("job/2", "running");
/// assert_eq!(kv.write(batch).unwrap(), 1);
/// kv.close().unwrap();
///
/// let kv = LogKv::open("data", Options::new().fs(fs)).unwrap();
/// assert_eq!(kv.scan(KeyRange::prefix("job/")).unwrap().len(), 2);
/// assert_eq!(kv.last_seq(), 1);
/// ```
pub struct LogKv {
    inner: Arc<Inner>,
    background: Mutex<Option<JoinHandle<()>>>,
}

impl LogKv {
    /// Opens (creating if needed) the store in `dir`, recovering its state.
    pub fn open(dir: impl AsRef<Path>, options: Options) -> Result<LogKv> {
        options.validate()?;
        let dir = dir.as_ref().to_path_buf();
        let fs = Arc::clone(&options.fs);
        if !fs.exists(&dir).map_err(io_err("checking", &dir))? {
            if !options.create_if_missing {
                return Err(StoreError::NotFound { dir });
            }
            fs.create_dir_all(&dir).map_err(io_err("creating", &dir))?;
        }
        let dir_lock = fs.lock(&dir.join(LOCK_NAME))?;
        let recovered = recovery::recover(&*fs, &dir, options.recovery)?;

        let next_seq = recovered.last_seq + 1;
        let active = SegmentWriter::create(&*fs, &dir, next_seq)?;
        let mut sealed = recovered.segments;
        // A leftover segment of that name held no batches; `create` has
        // just replaced it.
        sealed.remove(&next_seq);
        let retire_damage =
            recovered.stats.skipped_regions > 0 || recovered.stats.bad_snapshots > 0;

        let inner = Arc::new(Inner {
            dir,
            fs,
            sync: options.sync,
            segment_size: options.segment_size,
            checkpoint_bytes: options.checkpoint_bytes,
            visible: RwLock::new(Visible {
                table: recovered.table,
                seq: recovered.last_seq,
            }),
            commit: Mutex::new(CommitState::recovered(recovered.last_seq)),
            committed: Condvar::new(),
            wal: Mutex::new(Wal::new(active, sealed, recovered.last_seq)),
            checkpoint_lock: Mutex::new(()),
            checkpoint_seq: AtomicU64::new(recovered.snapshot_seq),
            snapshot_bytes: AtomicU64::new(recovered.snapshot_bytes),
            bg: Mutex::new(Background::default()),
            bg_signal: Condvar::new(),
            counters: Counters::default(),
            recovery: recovered.stats,
            closed: AtomicBool::new(false),
            dir_lock: Mutex::new(Some(dir_lock)),
        });

        let needs_thread =
            matches!(inner.sync, SyncPolicy::Interval(_)) || inner.checkpoint_bytes.is_some();
        let background = if needs_thread {
            let shared = Arc::clone(&inner);
            let handle = thread::Builder::new()
                .name("belay-store".into())
                .spawn(move || run_background(shared))
                .map_err(io_err("starting the background thread for", &inner.dir))?;
            Some(handle)
        } else {
            None
        };
        let kv = LogKv {
            inner,
            background: Mutex::new(background),
        };
        if retire_damage {
            // A fresh snapshot covers everything recovered, so the damaged
            // files are deleted or set aside and not met again.
            info!("writing a checkpoint to retire damaged store files");
            kv.inner.checkpoint(true)?;
        }
        Ok(kv)
    }

    /// The store directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// The sync policy in effect.
    #[must_use]
    pub fn sync_policy(&self) -> SyncPolicy {
        self.inner.sync
    }

    /// What recovery did when the store was opened.
    #[must_use]
    pub fn recovery(&self) -> &RecoveryStats {
        &self.inner.recovery
    }

    /// Whether [`close`](Self::close) has been called.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    /// Flushes the log and releases the directory lock. Writes that were
    /// already queued are completed first; later calls on this handle fail
    /// with [`StoreError::Closed`]. Closing twice is a no-op. Dropping the
    /// handle closes it too, but cannot report errors.
    pub fn close(&self) -> Result<()> {
        let inner = &self.inner;
        if inner.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        lock(&inner.bg).stop = true;
        inner.bg_signal.notify_all();
        if let Some(handle) = lock(&self.background).take() {
            if handle.join().is_err() {
                warn!("store background thread panicked");
            }
        }
        {
            let mut st = lock(&inner.commit);
            while st.leader || (!st.queue.is_empty() && st.failure.is_none()) {
                st = wait(&inner.committed, st);
            }
        }
        let _no_checkpoint = lock(&inner.checkpoint_lock);
        let failed = inner.failure().is_err();
        let result = {
            let mut wal = lock(&inner.wal);
            let seq = wal.written_seq;
            match wal.active.take() {
                Some(mut active) if !failed => active.sync().map(|_| seq),
                _ => Ok(0),
            }
        };
        match &result {
            Ok(seq) => inner.mark_durable(*seq),
            Err(e) => inner.fail(e),
        }
        lock(&inner.dir_lock).take();
        debug!("closed store in {}", inner.dir.display());
        result.map(|_| ())
    }
}

impl Drop for LogKv {
    fn drop(&mut self) {
        if let Err(e) = self.close() {
            warn!("closing store failed"; error = %e);
        }
    }
}

impl fmt::Debug for LogKv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LogKv")
            .field("dir", &self.inner.dir)
            .field("sync", &self.inner.sync)
            .field("seq", &read(&self.inner.visible).seq)
            .field("closed", &self.is_closed())
            .finish()
    }
}

impl Kv for LogKv {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.inner.ensure_open()?;
        Ok(read(&self.inner.visible).table.get(key).cloned())
    }

    fn scan(&self, range: KeyRange) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.inner.ensure_open()?;
        Ok(range.scan_map(read(&self.inner.visible).table.map()))
    }

    fn scan_keys(&self, range: KeyRange) -> Result<Vec<Vec<u8>>> {
        self.inner.ensure_open()?;
        Ok(range.scan_map_keys(read(&self.inner.visible).table.map()))
    }

    fn count(&self, range: KeyRange) -> Result<usize> {
        self.inner.ensure_open()?;
        Ok(range.count_map(read(&self.inner.visible).table.map()))
    }

    fn write(&self, batch: WriteBatch) -> Result<u64> {
        self.inner.commit(batch)
    }

    fn flush(&self) -> Result<()> {
        self.inner.ensure_open()?;
        self.inner.sync_wal()
    }

    fn stats(&self) -> KvStats {
        let inner = &self.inner;
        let (keys, live_bytes, last_seq) = {
            let visible = read(&inner.visible);
            (
                visible.table.len() as u64,
                visible.table.live_bytes(),
                visible.seq,
            )
        };
        let durable_seq = lock(&inner.commit).durable_seq;
        let (wal_segments, wal_bytes) = {
            let wal = lock(&inner.wal);
            (wal.segment_count(), wal.live_bytes())
        };
        let c = &inner.counters;
        KvStats {
            keys,
            live_bytes,
            last_seq,
            durable_seq,
            batches: Counters::get(&c.batches),
            ops: Counters::get(&c.ops),
            commit_groups: Counters::get(&c.groups),
            wal_syncs: Counters::get(&c.syncs),
            wal_bytes_written: Counters::get(&c.bytes),
            wal_segments,
            wal_bytes,
            checkpoints: Counters::get(&c.checkpoints),
            checkpoint_seq: inner.checkpoint_seq.load(Ordering::Acquire),
            snapshot_bytes: inner.snapshot_bytes.load(Ordering::Acquire),
            recovery: inner.recovery.clone(),
        }
    }

    fn checkpoint(&self) -> Result<CheckpointInfo> {
        self.inner.checkpoint(false)
    }

    fn last_seq(&self) -> u64 {
        read(&self.inner.visible).seq
    }
}
