//! The queue of membership updates waiting to be piggybacked.
//!
//! Each update is sent a bounded number of times (the retransmit limit,
//! which grows with log n) and then forgotten; anti-entropy catches
//! whatever gossip missed. Packets take the updates that have been sent
//! the fewest times first, newest first among equals, so fresh news spreads
//! before old news is repeated. A newer update about a member replaces the
//! queued one: there is no point spreading a state that is already stale.
//!
//! Updates are stored encoded, so packing a packet is a matter of comparing
//! byte lengths against the space left.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::member::NodeId;

/// Smallest possible encoded update (tag, 1-byte id, incarnation); once
/// less space than this is left, no further update can fit.
const MIN_UPDATE_LEN: usize = 4;

/// Position in the transmit order: fewest transmits first, then newest.
type Rank = (u32, Reverse<u64>);

#[derive(Debug)]
struct Entry {
    node: NodeId,
    bytes: Arc<[u8]>,
}

/// Transmit-limited broadcast queue, one entry per member.
#[derive(Debug, Default)]
pub(crate) struct GossipQueue {
    order: BTreeMap<Rank, Entry>,
    by_node: HashMap<NodeId, Rank>,
    next_id: u64,
}

impl GossipQueue {
    pub(crate) fn new() -> GossipQueue {
        GossipQueue::default()
    }

    /// Queues an encoded update about `node`, replacing any queued update
    /// about the same member.
    pub(crate) fn push(&mut self, node: NodeId, bytes: Vec<u8>) {
        if let Some(rank) = self.by_node.remove(&node) {
            self.order.remove(&rank);
        }
        let rank = (0, Reverse(self.next_id));
        self.next_id += 1;
        self.by_node.insert(node.clone(), rank);
        self.order.insert(
            rank,
            Entry {
                node,
                bytes: bytes.into(),
            },
        );
    }

    /// Takes updates totalling at most `budget` bytes for one packet and
    /// counts the transmission; entries reaching `limit` transmissions are
    /// dropped.
    pub(crate) fn select(&mut self, budget: usize, limit: u32) -> Vec<Arc<[u8]>> {
        let mut remaining = budget;
        let mut picked: Vec<Rank> = Vec::new();
        for (rank, entry) in &self.order {
            if remaining < MIN_UPDATE_LEN {
                break;
            }
            // Keep scanning past an update that does not fit: a smaller
            // one further down the order may.
            if entry.bytes.len() <= remaining {
                remaining -= entry.bytes.len();
                picked.push(*rank);
            }
        }
        let mut out = Vec::with_capacity(picked.len());
        for rank in picked {
            let entry = self
                .order
                .remove(&rank)
                .expect("picked ranks come from the order map");
            out.push(Arc::clone(&entry.bytes));
            let transmits = rank.0 + 1;
            if transmits >= limit {
                self.by_node.remove(&entry.node);
            } else {
                let next = (transmits, rank.1);
                self.by_node.insert(entry.node.clone(), next);
                self.order.insert(next, entry);
            }
        }
        out
    }

    /// Whether an update about `node` is still queued.
    pub(crate) fn contains(&self, node: &str) -> bool {
        self.by_node.contains_key(node)
    }

    pub(crate) fn len(&self) -> usize {
        self.order.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> NodeId {
        NodeId::new(s).unwrap()
    }

    fn bytes(tag: u8, len: usize) -> Vec<u8> {
        vec![tag; len]
    }

    fn tags(selected: &[Arc<[u8]>]) -> Vec<u8> {
        selected.iter().map(|b| b[0]).collect()
    }

    #[test]
    fn newer_update_about_a_member_replaces_the_queued_one() {
        let mut q = GossipQueue::new();
        q.push(id("a"), bytes(1, 10));
        q.push(id("b"), bytes(2, 10));
        q.push(id("a"), bytes(3, 10));
        assert_eq!(q.len(), 2);
        assert_eq!(tags(&q.select(1_000, 10)), [3, 2]);
    }

    #[test]
    fn least_transmitted_then_newest_go_first() {
        let mut q = GossipQueue::new();
        q.push(id("a"), bytes(1, 10));
        q.push(id("b"), bytes(2, 10));
        // Room for one: the newest.
        assert_eq!(tags(&q.select(10, 10)), [2]);
        // "a" has now been sent fewer times than "b".
        assert_eq!(tags(&q.select(10, 10)), [1]);
        q.push(id("c"), bytes(3, 10));
        assert_eq!(tags(&q.select(30, 10)), [3, 2, 1]);
    }

    #[test]
    fn entries_are_dropped_after_the_retransmit_limit() {
        let mut q = GossipQueue::new();
        q.push(id("a"), bytes(1, 10));
        for _ in 0..3 {
            assert_eq!(q.select(100, 3).len(), 1);
        }
        assert!(q.is_empty());
        assert!(!q.contains("a"));
        assert!(q.select(100, 3).is_empty());
    }

    #[test]
    fn respects_the_byte_budget_and_skips_oversized_entries() {
        let mut q = GossipQueue::new();
        q.push(id("small-old"), bytes(1, 20));
        q.push(id("big"), bytes(2, 100));
        q.push(id("small-new"), bytes(3, 30));
        let picked = q.select(60, 10);
        assert_eq!(tags(&picked), [3, 1]);
        assert!(picked.iter().map(|b| b.len()).sum::<usize>() <= 60);
        // The big one was not counted as sent and goes first next time.
        assert_eq!(tags(&q.select(100, 10)), [2]);
        assert!(q.select(MIN_UPDATE_LEN - 1, 10).is_empty());
    }

    #[test]
    fn replacing_resets_the_transmit_count() {
        let mut q = GossipQueue::new();
        q.push(id("a"), bytes(1, 10));
        q.select(100, 2);
        q.push(id("a"), bytes(2, 10));
        assert_eq!(tags(&q.select(100, 2)), [2]);
        assert!(q.contains("a"), "one transmission left after the reset");
        q.select(100, 2);
        assert!(!q.contains("a"));
    }
}
