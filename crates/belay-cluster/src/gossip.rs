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
