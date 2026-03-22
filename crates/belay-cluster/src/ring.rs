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

/// A set of weighted nodes that keys are assigned to.
///
/// ```
/// use belay_cluster::{NodeId, Ring};
///
/// let mut ring = Ring::new();
/// for id in ["node-a", "node-b", "node-c"] {
///     ring.insert(NodeId::new(id).unwrap(), 1.0).unwrap();
/// }
/// let owner = ring.owner("queue:emails").unwrap().clone();
///
/// // Removing some other node never moves the key.
/// let other = ["node-a", "node-b", "node-c"].into_iter().find(|n| *n != owner.as_str()).unwrap();
/// ring.remove(other);
/// assert_eq!(ring.owner("queue:emails"), Some(&owner));
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ring {
    /// Sorted by id, so that iteration and ties are deterministic.
    nodes: Vec<RingNode>,
}

/// A node's score for one key; higher wins.
#[derive(Clone, Copy, Debug)]
struct Score {
    /// `weight / -ln(u)`, or 0 in uniform rings (compare `hash` only).
    weighted: f64,
    hash: u64,
}

impl Ring {
    /// An empty ring.
    #[must_use]
    pub fn new() -> Ring {
        Ring::default()
    }

    /// A ring of the live members among `members`, each with its
    /// [`weight`](Member::weight).
    #[must_use]
    pub fn from_members<'a, I>(members: I) -> Ring
    where
        I: IntoIterator<Item = &'a Member>,
    {
        let mut ring = Ring::new();
        for member in members.into_iter().filter(|m| m.is_live()) {
            ring.insert(member.id.clone(), member.weight())
                .expect("Member::weight returns only weights that pass validation");
        }
        ring
    }

    /// Adds a node, or changes the weight of one already present.
    /// `weight` must be finite and in `(0, 1000]`.
    pub fn insert(&mut self, id: NodeId, weight: f64) -> Result<()> {
        if !(weight.is_finite() && weight > 0.0 && weight <= MAX_WEIGHT) {
            return Err(Error::InvalidWeight(weight));
        }
        match self.nodes.binary_search_by(|n| n.id.cmp(&id)) {
            Ok(i) => self.nodes[i].weight = weight,
            Err(i) => {
                let seed = hash64(id.as_str().as_bytes(), NODE_SEED);
                self.nodes.insert(i, RingNode { id, weight, seed });
            }
        }
        Ok(())
    }

    /// Removes a node; returns whether it was present.
    pub fn remove(&mut self, id: &str) -> bool {
        match self.position(id) {
            Some(i) => {
                self.nodes.remove(i);
                true
            }
            None => false,
        }
    }

    fn position(&self, id: &str) -> Option<usize> {
        self.nodes.binary_search_by(|n| n.id.as_str().cmp(id)).ok()
    }

    /// Whether `id` is in the ring.
    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.position(id).is_some()
    }

    /// The weight of `id`, if present.
    #[must_use]
    pub fn weight(&self, id: &str) -> Option<f64> {
        self.position(id).map(|i| self.nodes[i].weight)
    }

    /// Number of nodes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the ring has no nodes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The nodes, in id order.
    pub fn nodes(&self) -> impl Iterator<Item = &NodeId> + '_ {
        self.nodes.iter().map(|n| &n.id)
    }

    fn is_uniform(&self) -> bool {
        self.nodes
            .windows(2)
            .all(|pair| pair[0].weight == pair[1].weight)
    }

    fn score(node: &RingNode, key: &[u8], uniform: bool) -> Score {
        let hash = hash64(key, node.seed);
        let weighted = if uniform {
            0.0
        } else {
            // The top 53 bits, centred in their interval: u is in (0, 1),
            // never exactly 0 or 1, so the logarithm is finite and negative.
            let u = ((hash >> 11) as f64 + 0.5) / (1u64 << 53) as f64;
            node.weight / -u.ln()
        };
        Score { weighted, hash }
    }

    /// Higher score first; ties (practically only equal hashes) go to the
    /// lower id so that every node breaks them the same way.
    fn rank(a: (&RingNode, Score), b: (&RingNode, Score)) -> Ordering {
        b.1.weighted
            .total_cmp(&a.1.weighted)
            .then(b.1.hash.cmp(&a.1.hash))
            .then_with(|| a.0.id.cmp(&b.0.id))
    }

    /// The node that owns `key`, or `None` if the ring is empty.
    #[must_use]
    pub fn owner(&self, key: impl AsRef<[u8]>) -> Option<&NodeId> {
        let key = key.as_ref();
        let uniform = self.is_uniform();
        self.nodes
            .iter()
            .map(|n| (n, Ring::score(n, key, uniform)))
            .min_by(|a, b| Ring::rank(*a, *b))
            .map(|(n, _)| &n.id)
    }

    /// The `n` highest-scoring nodes for `key`, best first: the owner, then
    /// the node that would take over if the owner went away, and so on.
    /// Fewer if the ring has fewer nodes.
    #[must_use]
    pub fn owners(&self, key: impl AsRef<[u8]>, n: usize) -> Vec<&NodeId> {
        let key = key.as_ref();
        let uniform = self.is_uniform();
        let mut scored: Vec<(&RingNode, Score)> = self
            .nodes
            .iter()
            .map(|node| (node, Ring::score(node, key, uniform)))
            .collect();
        scored.sort_by(|a, b| Ring::rank(*a, *b));
        scored
            .into_iter()
            .take(n)
            .map(|(node, _)| &node.id)
            .collect()
    }
}

/// One key whose owner differs between two rings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Move {
    /// The key.
    pub key: String,
    /// The previous owner (`None` if the old ring was empty).
    pub from: Option<NodeId>,
    /// The new owner (`None` if the new ring is empty).
    pub to: Option<NodeId>,
}

/// The difference in ownership of a set of keys between two rings, e.g.
/// before and after a membership change.
///
/// ```
/// use belay_cluster::{NodeId, Ownership, Ring};
///
/// let id = |s: &str| NodeId::new(s).unwrap();
/// let mut before = Ring::new();
/// before.insert(id("a"), 1.0).unwrap();
/// before.insert(id("b"), 1.0).unwrap();
/// let mut after = before.clone();
/// after.insert(id("c"), 1.0).unwrap();
///
/// let queues: Vec<String> = (0..300).map(|i| format!("queue-{i}")).collect();
/// let diff = Ownership::diff(&before, &after, &queues);
/// assert_eq!(diff.total(), 300);
/// // Only the new node gains keys.
/// assert!(diff.moves().iter().all(|m| m.to.as_ref().unwrap() == "c"));
/// assert_eq!(diff.gained_by("c").count(), diff.moved());
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ownership {
    moves: Vec<Move>,
    unchanged: usize,
}

impl Ownership {
    /// Compares the owners of `keys` in `before` and `after`.
    pub fn diff<I, K>(before: &Ring, after: &Ring, keys: I) -> Ownership
    where
        I: IntoIterator<Item = K>,
        K: AsRef<str>,
    {
        let mut diff = Ownership::default();
        for key in keys {
            let key = key.as_ref();
            let from = before.owner(key);
            let to = after.owner(key);
            if from == to {
                diff.unchanged += 1;
            } else {
                diff.moves.push(Move {
                    key: key.to_owned(),
                    from: from.cloned(),
                    to: to.cloned(),
                });
            }
        }
        diff
    }

    /// The keys that changed owner, in input order.
    #[must_use]
    pub fn moves(&self) -> &[Move] {
        &self.moves
    }

    /// Number of keys that changed owner.
    #[must_use]
    pub fn moved(&self) -> usize {
        self.moves.len()
    }

    /// Number of keys that kept their owner.
    #[must_use]
    pub fn unchanged(&self) -> usize {
        self.unchanged
    }

    /// Number of keys compared.
    #[must_use]
    pub fn total(&self) -> usize {
        self.moves.len() + self.unchanged
    }

    /// Fraction of keys that moved (0 when no keys were compared).
    #[must_use]
    pub fn moved_fraction(&self) -> f64 {
        match self.total() {
            0 => 0.0,
            total => self.moved() as f64 / total as f64,
        }
    }

    /// Whether no key moved.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.moves.is_empty()
    }

    /// Keys that `node` owns after the change but did not before.
    pub fn gained_by<'a>(&'a self, node: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.moves
            .iter()
            .filter(move |m| m.to.as_ref().is_some_and(|to| to == node))
            .map(|m| m.key.as_str())
    }

    /// Keys that `node` owned before the change but no longer does.
    pub fn lost_by<'a>(&'a self, node: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.moves
            .iter()
            .filter(move |m| m.from.as_ref().is_some_and(|from| from == node))
            .map(|m| m.key.as_str())
    }

    /// How many of the moved keys each new owner gained.
    #[must_use]
    pub fn gains(&self) -> BTreeMap<&NodeId, usize> {
        let mut gains = BTreeMap::new();
        for to in self.moves.iter().filter_map(|m| m.to.as_ref()) {
            *gains.entry(to).or_insert(0) += 1;
        }
        gains
    }
}

#[cfg(test)]
mod tests;
