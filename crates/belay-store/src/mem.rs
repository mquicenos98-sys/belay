//! A store that lives only in memory.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::RwLock;

use belay_common::sync::{read, write};

use crate::batch::WriteBatch;
use crate::error::Result;
use crate::kv::{CheckpointInfo, Kv, KvStats};
use crate::range::KeyRange;
use crate::table::Table;

#[derive(Debug, Default)]
struct State {
    table: Table,
    seq: u64,
    batches: u64,
    ops: u64,
}

/// A [`Kv`] backed by a `BTreeMap` and nothing else: for tests and
/// `belay dev --in-memory`. Everything is lost when it is dropped;
/// [`flush`](Kv::flush) and [`checkpoint`](Kv::checkpoint) do nothing.
///
/// ```
/// use belay_store::{KeyRange, Kv, MemKv, WriteBatch};
///
/// let kv = MemKv::new();
/// let mut batch = WriteBatch::new();
/// batch.put("job/1", "a").put("job/2", "b").put("queue/x", "c");
/// kv.write(batch).unwrap();
/// let jobs = kv.scan(KeyRange::prefix(b"job/").reverse()).unwrap();
/// assert_eq!(jobs[0].0, b"job/2");
/// ```
#[derive(Default)]
pub struct MemKv {
    state: RwLock<State>,
}

impl MemKv {
    /// An empty store.
    #[must_use]
    pub fn new() -> MemKv {
        MemKv::default()
    }

    /// A store pre-loaded with `entries`, at sequence number 0.
    #[must_use]
    pub fn from_entries<K, V>(entries: impl IntoIterator<Item = (K, V)>) -> MemKv
    where
        K: Into<Vec<u8>>,
        V: Into<Vec<u8>>,
    {
        let map: BTreeMap<Vec<u8>, Vec<u8>> = entries
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        MemKv {
            state: RwLock::new(State {
                table: Table::from_map(map),
                ..State::default()
            }),
        }
    }

    /// Number of keys.
    #[must_use]
    pub fn len(&self) -> usize {
        read(&self.state).table.len()
    }

    /// Whether the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A copy of every entry.
    #[must_use]
    pub fn to_map(&self) -> BTreeMap<Vec<u8>, Vec<u8>> {
        read(&self.state).table.map().clone()
    }
}

impl fmt::Debug for MemKv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = read(&self.state);
        f.debug_struct("MemKv")
            .field("keys", &state.table.len())
            .field("seq", &state.seq)
            .finish()
    }
}

impl Kv for MemKv {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(read(&self.state).table.get(key).cloned())
    }

    fn scan(&self, range: KeyRange) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        Ok(range.scan_map(read(&self.state).table.map()))
    }

    fn scan_keys(&self, range: KeyRange) -> Result<Vec<Vec<u8>>> {
        Ok(range.scan_map_keys(read(&self.state).table.map()))
    }

    fn count(&self, range: KeyRange) -> Result<usize> {
        Ok(range.count_map(read(&self.state).table.map()))
    }

    fn write(&self, batch: WriteBatch) -> Result<u64> {
        batch.validate()?;
        let mut state = write(&self.state);
        if batch.is_empty() {
            return Ok(state.seq);
        }
        state.seq += 1;
        state.batches += 1;
        state.ops += batch.len() as u64;
        state.table.apply(batch);
        Ok(state.seq)
    }

    fn flush(&self) -> Result<()> {
        Ok(())
    }

    fn stats(&self) -> KvStats {
        let state = read(&self.state);
        KvStats {
            keys: state.table.len() as u64,
            live_bytes: state.table.live_bytes(),
            last_seq: state.seq,
            batches: state.batches,
            ops: state.ops,
            ..KvStats::default()
        }
    }

    fn checkpoint(&self) -> Result<CheckpointInfo> {
        let state = read(&self.state);
        Ok(CheckpointInfo {
            seq: state.seq,
            keys: state.table.len() as u64,
            ..CheckpointInfo::default()
        })
    }

    fn last_seq(&self) -> u64 {
        read(&self.state).seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StoreError;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn writes_get_consecutive_sequence_numbers() {
        let kv = MemKv::new();
        let mut b = WriteBatch::new();
        b.put("a", "1");
        assert_eq!(kv.write(b.clone()).expect("write"), 1);
        assert_eq!(kv.write(b).expect("write"), 2);
        assert_eq!(kv.write(WriteBatch::new()).expect("empty write"), 2);
        let stats = kv.stats();
        assert_eq!((stats.last_seq, stats.batches, stats.ops), (2, 2, 2));
        assert_eq!(stats.durable_seq, 0);
        assert_eq!(stats.live_bytes, 2);
    }

    #[test]
    fn oversized_writes_are_rejected_without_a_sequence_number() {
        let kv = MemKv::new();
        let mut b = WriteBatch::new();
        b.put(vec![0u8; crate::MAX_KEY_LEN + 1], "");
        assert!(matches!(kv.write(b), Err(StoreError::TooLarge { .. })));
        assert_eq!(kv.last_seq(), 0);
        assert!(kv.is_empty());
    }

    #[test]
    fn from_entries_and_checkpoint() {
        let kv = MemKv::from_entries([("b", "2"), ("a", "1")]);
        assert_eq!(kv.len(), 2);
        assert_eq!(kv.scan_keys(KeyRange::all()).expect("keys")[0], b"a");
        assert_eq!(kv.count(KeyRange::prefix("a")).expect("count"), 1);
        let info = kv.checkpoint().expect("checkpoint");
        assert!(!info.written);
        assert_eq!(info.keys, 2);
        kv.flush().expect("flush");
        assert!(format!("{kv:?}").contains("keys: 2"));
    }

    #[test]
    fn concurrent_batches_are_atomic_to_readers() {
        // Each batch moves one unit between two keys; readers must always
        // see the invariant a + b == 100.
        let kv = Arc::new(MemKv::new());
        let mut init = WriteBatch::new();
        init.put("a", 100u32.to_be_bytes())
            .put("b", 0u32.to_be_bytes());
        kv.write(init).expect("init");
        let writer = {
            let kv = Arc::clone(&kv);
            thread::spawn(move || {
                for i in 1..=500u32 {
                    let mut b = WriteBatch::new();
                    b.put("a", (100 - i % 101).to_be_bytes())
                        .put("b", (i % 101).to_be_bytes());
                    kv.write(b).expect("write");
                }
            })
        };
        let decode = |v: &[u8]| u32::from_be_bytes(v.try_into().expect("4 bytes"));
        for _ in 0..500 {
            let entries = kv.scan(KeyRange::all()).expect("scan");
            let sum: u32 = entries.iter().map(|(_, v)| decode(v)).sum();
            assert_eq!(sum, 100);
        }
        writer.join().expect("writer");
    }
}
