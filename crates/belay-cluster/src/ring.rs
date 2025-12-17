//! Key ownership by rendezvous (highest random weight) hashing.
//!
//! Every node scores every key with a hash of the pair, and the key belongs
//! to the highest-scoring node (Thaler & Ravishankar, 1998). Compared with
//! a hash ring of virtual nodes this needs no tuning and no state beyond
//! the member list: when a node joins it takes over exactly the keys for
//! which it now scores highest, about `1/(n+1)` of them, and when a node
//! leaves only its own keys move, spread evenly over the survivors. Lookup
//! costs one hash per node, which is nothing next to a network round trip
//! for the few hundred nodes a cluster like this can have.
//!
//! Scores use XXH64 seeded per node, so they are identical on every
//! platform. Weighted rings use the logarithmic method (Schindelhauer &
//! Schomaker, 2005): with the hash mapped to `u` in (0, 1), the score is
//! `weight / -ln(u)`, which gives each node a share of keys proportional
//! to its weight while keeping the minimal-movement property. That one
//! step goes through `f64::ln`, whose last bit may differ between math
//! libraries; nodes on different platforms could then disagree about a key
//! whose two best scores are within one rounding error of each other, a
//! chance of roughly 2^-50 per key. Rings whose weights are all equal skip
//! the floating point entirely and compare the integer hashes.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use belay_common::hash::hash64;

use crate::error::{Error, Result};
use crate::member::{Member, NodeId, MAX_WEIGHT};

/// Seed for deriving each node's scoring seed from its id.
const NODE_SEED: u64 = 0x6265_6c61_795f_6872;

#[derive(Clone, Debug, PartialEq)]
struct RingNode {
    id: NodeId,
    weight: f64,
    seed: u64,
}
