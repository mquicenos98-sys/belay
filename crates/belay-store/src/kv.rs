//! The storage interface the rest of belay programs against.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::batch::WriteBatch;
use crate::error::Result;
use crate::range::KeyRange;

/// A sorted, byte-oriented key-value store with atomic batch writes.
///
/// Implementations are shared between threads (`Arc<dyn Kv>`). Every
/// successful [`write`](Kv::write) is assigned the next sequence number,
/// starting at 1, and is visible to reads on any thread once `write`
/// returns. Reads never observe part of a batch.
pub trait Kv: Send + Sync + fmt::Debug {
    /// The value stored under `key`.
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;

    /// The entries within `range`, in key order (descending if the range
    /// is [reversed](KeyRange::reverse)), truncated to its limit. The result
    /// is a consistent snapshot: it reflects every write that returned
    /// before the scan started and no partial batch.
    fn scan(&self, range: KeyRange) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;

    /// Applies `batch` atomically and returns its sequence number. An empty
    /// batch writes nothing and returns the latest sequence number.
    fn write(&self, batch: WriteBatch) -> Result<u64>;

    /// Makes every write that has returned durable, whatever the sync
    /// policy.
    fn flush(&self) -> Result<()>;

    /// Counters and sizes, for metrics and `belay admin store`.
    fn stats(&self) -> KvStats;

    /// Persists the current state in a form that makes the next open fast
    /// and lets the store discard log history. A no-op for stores without
    /// durable state.
    fn checkpoint(&self) -> Result<CheckpointInfo>;

    /// Like [`scan`](Kv::scan) but returns only keys.
    fn scan_keys(&self, range: KeyRange) -> Result<Vec<Vec<u8>>> {
        Ok(self.scan(range)?.into_iter().map(|(k, _)| k).collect())
    }

    /// How many entries [`scan`](Kv::scan) would return for `range`.
    fn count(&self, range: KeyRange) -> Result<usize> {
        Ok(self.scan_keys(range)?.len())
    }

    /// Whether `key` has a value.
    fn contains(&self, key: &[u8]) -> Result<bool> {
        Ok(self.get(key)?.is_some())
    }

    /// Writes a single key.
    fn put(&self, key: &[u8], value: &[u8]) -> Result<u64> {
        let mut batch = WriteBatch::with_capacity(1);
        batch.put(key, value);
        self.write(batch)
    }

    /// Deletes a single key.
    fn delete(&self, key: &[u8]) -> Result<u64> {
        let mut batch = WriteBatch::with_capacity(1);
        batch.delete(key);
        self.write(batch)
    }

    /// Sequence number of the latest visible write (0 before the first).
    fn last_seq(&self) -> u64 {
        self.stats().last_seq
    }
}

impl<T: Kv + ?Sized> Kv for Arc<T> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        (**self).get(key)
    }
    fn scan(&self, range: KeyRange) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        (**self).scan(range)
    }
    fn write(&self, batch: WriteBatch) -> Result<u64> {
        (**self).write(batch)
    }
    fn flush(&self) -> Result<()> {
        (**self).flush()
    }
    fn stats(&self) -> KvStats {
        (**self).stats()
    }
    fn checkpoint(&self) -> Result<CheckpointInfo> {
        (**self).checkpoint()
    }
    fn scan_keys(&self, range: KeyRange) -> Result<Vec<Vec<u8>>> {
        (**self).scan_keys(range)
    }
    fn count(&self, range: KeyRange) -> Result<usize> {
        (**self).count(range)
    }
    fn contains(&self, key: &[u8]) -> Result<bool> {
        (**self).contains(key)
    }
    fn put(&self, key: &[u8], value: &[u8]) -> Result<u64> {
        (**self).put(key, value)
    }
    fn delete(&self, key: &[u8]) -> Result<u64> {
        (**self).delete(key)
    }
    fn last_seq(&self) -> u64 {
        (**self).last_seq()
    }
}

/// Store statistics. Counters marked "since open" restart at zero when the
/// store is reopened.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct KvStats {
    /// Number of keys.
    pub keys: u64,
    /// Sum of key and value lengths.
    pub live_bytes: u64,
    /// Sequence number of the latest visible write.
    pub last_seq: u64,
    /// Highest sequence number known to be on stable storage (always 0 for
    /// [`MemKv`](crate::MemKv)).
    pub durable_seq: u64,
    /// Batches written since open.
    pub batches: u64,
    /// Operations in those batches.
    pub ops: u64,
    /// Group commits since open: log writes that carried one or more
    /// batches. `batches / commit_groups` is the average group size.
    pub commit_groups: u64,
    /// Log syncs since open.
    pub wal_syncs: u64,
    /// Bytes appended to the log since open.
    pub wal_bytes_written: u64,
    /// Log segment files on disk.
    pub wal_segments: u64,
    /// Total size of those segment files.
    pub wal_bytes: u64,
    /// Checkpoints written since open.
    pub checkpoints: u64,
    /// Sequence number covered by the newest snapshot (0 if none).
    pub checkpoint_seq: u64,
    /// Size of the newest snapshot file.
    pub snapshot_bytes: u64,
    /// What happened when the store was opened.
    pub recovery: RecoveryStats,
}

/// What recovery found and did when a store was opened.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RecoveryStats {
    /// Sequence number of the snapshot recovery started from (0 if none).
    pub snapshot_seq: u64,
    /// Keys loaded from that snapshot.
    pub snapshot_keys: u64,
    /// Snapshots passed over because they failed validation.
    pub bad_snapshots: u64,
    /// Log segments read.
    pub segments_replayed: u64,
    /// Batches applied from the log.
    pub batches_replayed: u64,
    /// Torn log tails that were truncated (0 or 1).
    pub torn_tails: u64,
    /// Bytes removed by that truncation.
    pub torn_bytes: u64,
    /// Damaged log regions skipped under `RecoveryMode::SkipCorrupt`.
    pub skipped_regions: u64,
    /// Bytes in those regions.
    pub skipped_bytes: u64,
    /// Leftover files deleted: temporary snapshots, superseded snapshots
    /// and log segments covered by the snapshot.
    pub removed_files: u64,
    /// Wall-clock time recovery took.
    pub duration: Duration,
}
