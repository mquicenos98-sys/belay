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

impl RecoveryStats {
    /// Whether recovery had to discard or skip anything.
    #[must_use]
    pub fn lost_data(&self) -> bool {
        self.torn_tails > 0 || self.skipped_regions > 0 || self.bad_snapshots > 0
    }
}

/// Outcome of [`Kv::checkpoint`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckpointInfo {
    /// Sequence number the snapshot covers.
    pub seq: u64,
    /// Keys in the snapshot.
    pub keys: u64,
    /// Snapshot file size.
    pub snapshot_bytes: u64,
    /// Log segments deleted because the snapshot covers them.
    pub segments_removed: u64,
    /// Whether a snapshot was written; `false` when nothing changed since
    /// the previous checkpoint or the store has no durable state.
    pub written: bool,
}

impl fmt::Display for CheckpointInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.written {
            write!(
                f,
                "checkpoint at seq {}: {} keys, {} snapshot bytes, {} log segments removed",
                self.seq, self.keys, self.snapshot_bytes, self.segments_removed
            )
        } else {
            write!(f, "no checkpoint needed at seq {}", self.seq)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemKv;

    #[test]
    fn provided_methods_work_through_an_arc() {
        let kv: Arc<dyn Kv> = Arc::new(MemKv::new());
        assert_eq!(kv.put(b"a", b"1").expect("put"), 1);
        assert_eq!(kv.put(b"b", b"2").expect("put"), 2);
        assert!(kv.contains(b"a").expect("contains"));
        assert_eq!(kv.count(KeyRange::all()).expect("count"), 2);
        assert_eq!(kv.delete(b"a").expect("delete"), 3);
        assert_eq!(
            kv.scan_keys(KeyRange::all()).expect("keys"),
            vec![b"b".to_vec()]
        );
        assert_eq!(kv.last_seq(), 3);
        let shared = Arc::new(Arc::clone(&kv));
        assert_eq!(shared.get(b"b").expect("get"), Some(b"2".to_vec()));
    }

    #[test]
    fn checkpoint_info_describes_itself() {
        let info = CheckpointInfo {
            seq: 9,
            keys: 3,
            snapshot_bytes: 120,
            segments_removed: 2,
            written: true,
        };
        assert_eq!(
            info.to_string(),
            "checkpoint at seq 9: 3 keys, 120 snapshot bytes, 2 log segments removed"
        );
        let skipped = CheckpointInfo {
            seq: 9,
            ..CheckpointInfo::default()
        };
        assert_eq!(skipped.to_string(), "no checkpoint needed at seq 9");
    }

    #[test]
    fn recovery_stats_flag_data_loss() {
        assert!(!RecoveryStats::default().lost_data());
        let torn = RecoveryStats {
            torn_tails: 1,
            ..RecoveryStats::default()
        };
        assert!(torn.lost_data());
    }
}
