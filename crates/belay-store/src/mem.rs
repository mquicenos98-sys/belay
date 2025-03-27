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
