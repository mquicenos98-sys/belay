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

#[test]
fn empty_ring_owns_nothing() {
    let ring = Ring::new();
    assert!(ring.is_empty());
    assert_eq!(ring.owner("k"), None);
    assert!(ring.owners("k", 3).is_empty());
}

#[test]
fn placement_is_pinned_across_versions() {
    // Every node must compute the same owners, including nodes running
    // other builds of belay: changing the scoring function in any way is a
    // breaking protocol change and must fail this test.
    let ring = ring(&["node-a", "node-b", "node-c"]);
    let owners: Vec<&str> = ["emails", "reports", "billing", "sched:nightly", ""]
        .iter()
        .map(|k| ring.owner(k).unwrap().as_str())
        .collect();
    assert_eq!(owners, PINNED_OWNERS);
}

/// Owners of the keys in `placement_is_pinned_across_versions`.
const PINNED_OWNERS: [&str; 5] = ["node-a", "node-c", "node-b", "node-b", "node-b"];

#[test]
fn owner_does_not_depend_on_insertion_order() {
    let a = ring(&["x", "y", "z", "w"]);
    let b = ring(&["w", "z", "y", "x"]);
    assert_eq!(a, b);
    for key in keys(200) {
        assert_eq!(a.owner(&key), b.owner(&key));
    }
}

#[test]
fn keys_spread_evenly_over_equal_nodes() {
    let ids = names(10);
    let ring = ring(&ids.iter().map(String::as_str).collect::<Vec<_>>());
    let keys = keys(20_000);
    let counts = counts(&ring, &keys);
    assert_eq!(counts.len(), 10);
    for (node, count) in counts {
        assert!((1_700..=2_300).contains(&count), "{node} owns {count}");
    }
}
