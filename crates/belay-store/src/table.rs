//! The in-memory sorted map both stores serve reads from.

use std::collections::BTreeMap;

use crate::batch::{Op, WriteBatch};

/// A sorted map plus the bookkeeping the statistics need.
#[derive(Clone, Debug, Default)]
pub(crate) struct Table {
    map: BTreeMap<Vec<u8>, Vec<u8>>,
    /// Sum of key and value lengths.
    live_bytes: u64,
}

impl Table {
    pub(crate) fn new() -> Table {
        Table::default()
    }

    pub(crate) fn from_map(map: BTreeMap<Vec<u8>, Vec<u8>>) -> Table {
        let live_bytes = map.iter().map(|(k, v)| (k.len() + v.len()) as u64).sum();
        Table { map, live_bytes }
    }

    pub(crate) fn map(&self) -> &BTreeMap<Vec<u8>, Vec<u8>> {
        &self.map
    }

    pub(crate) fn get(&self, key: &[u8]) -> Option<&Vec<u8>> {
        self.map.get(key)
    }

    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    pub(crate) fn live_bytes(&self) -> u64 {
        self.live_bytes
    }

    /// Applies every operation of `batch` in order.
    pub(crate) fn apply(&mut self, batch: WriteBatch) {
        for op in batch {
            self.apply_op(op);
        }
    }

    fn apply_op(&mut self, op: Op) {
        match op {
            Op::Put { key, value } => {
                let added = (key.len() + value.len()) as u64;
                if let Some(old) = self.map.get_mut(&key) {
                    self.live_bytes -= old.len() as u64;
                    self.live_bytes += value.len() as u64;
                    *old = value;
                } else {
                    self.live_bytes += added;
                    self.map.insert(key, value);
                }
            }
            Op::Delete { key } => self.remove(&key),
            ref range @ Op::DeleteRange { .. } => {
                let Some(bounds) = range.range_bounds() else {
                    return;
                };
                // Collecting the keys first costs O(k log n) for k removed
                // keys; splitting and re-appending the map would be O(n).
                let doomed: Vec<Vec<u8>> = self
                    .map
                    .range::<[u8], _>(bounds)
                    .map(|(k, _)| k.clone())
                    .collect();
                for key in doomed {
                    self.remove(&key);
                }
            }
        }
    }

    fn remove(&mut self, key: &[u8]) {
        if let Some(old) = self.map.remove(key) {
            self.live_bytes -= (key.len() + old.len()) as u64;
        }
    }
}
