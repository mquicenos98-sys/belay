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
