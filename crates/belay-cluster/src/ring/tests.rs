use super::*;
use crate::member::{MemberState, Meta};
use belay_common::Timestamp;

fn id(s: &str) -> NodeId {
    NodeId::new(s).unwrap()
}

fn ring(ids: &[&str]) -> Ring {
    let mut ring = Ring::new();
    for name in ids {
        ring.insert(id(name), 1.0).unwrap();
    }
    ring
}

fn names(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("node-{i:02}")).collect()
}

fn keys(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("queue:{i}")).collect()
}

fn counts(ring: &Ring, keys: &[String]) -> BTreeMap<NodeId, usize> {
    let mut counts = BTreeMap::new();
    for key in keys {
        *counts.entry(ring.owner(key).unwrap().clone()).or_insert(0) += 1;
    }
    counts
}
