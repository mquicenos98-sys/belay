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

#[test]
fn a_joining_node_takes_about_one_in_n_plus_one_keys_and_only_for_itself() {
    let ids = names(10);
    let before = ring(&ids.iter().map(String::as_str).collect::<Vec<_>>());
    let mut after = before.clone();
    after.insert(id("node-new"), 1.0).unwrap();
    let keys = keys(20_000);
    let diff = Ownership::diff(&before, &after, &keys);
    let fraction = diff.moved_fraction();
    assert!((0.075..=0.107).contains(&fraction), "moved {fraction}");
    assert!(diff
        .moves()
        .iter()
        .all(|m| m.to.as_ref().is_some_and(|to| to == "node-new")));
    assert_eq!(diff.gained_by("node-new").count(), diff.moved());
    assert_eq!(diff.gains().len(), 1);
}

#[test]
fn a_departing_node_gives_up_only_its_own_keys_spread_over_the_rest() {
    let ids = names(10);
    let before = ring(&ids.iter().map(String::as_str).collect::<Vec<_>>());
    let mut after = before.clone();
    assert!(after.remove("node-03"));
    assert!(!after.remove("node-03"));
    let keys = keys(20_000);
    let owned_by_leaver = counts(&before, &keys)[&id("node-03")];
    let diff = Ownership::diff(&before, &after, &keys);
    assert_eq!(diff.moved(), owned_by_leaver);
    assert_eq!(diff.lost_by("node-03").count(), diff.moved());
    // The orphaned keys are shared by all nine survivors.
    let gains = diff.gains();
    assert_eq!(gains.len(), 9);
    for (node, gained) in gains {
        assert!(gained > owned_by_leaver / 18, "{node} gained only {gained}");
    }
}

#[test]
fn weights_scale_the_share_of_keys() {
    let mut ring = Ring::new();
    ring.insert(id("small"), 1.0).unwrap();
    ring.insert(id("medium"), 2.0).unwrap();
    ring.insert(id("large"), 5.0).unwrap();
    let counts = counts(&ring, &keys(40_000));
    let share = |n: &str| counts[&id(n)] as f64 / 40_000.0;
    assert!((share("small") - 0.125).abs() < 0.02, "{}", share("small"));
    assert!((share("medium") - 0.25).abs() < 0.02, "{}", share("medium"));
    assert!((share("large") - 0.625).abs() < 0.02, "{}", share("large"));
}

#[test]
fn weighted_rings_keep_minimal_movement() {
    let mut before = Ring::new();
    for (i, name) in names(6).iter().enumerate() {
        before.insert(id(name), 1.0 + i as f64).unwrap();
    }
    let mut after = before.clone();
    after.insert(id("node-new"), 3.0).unwrap();
    let diff = Ownership::diff(&before, &after, keys(10_000));
    assert!(diff
        .moves()
        .iter()
        .all(|m| m.to.as_ref().is_some_and(|to| to == "node-new")));
    // Expected share: 3 / (21 + 3).
    assert!(
        (diff.moved_fraction() - 0.125).abs() < 0.02,
        "{}",
        diff.moved_fraction()
    );
}

#[test]
fn uniform_weights_of_any_value_agree_with_unit_weights() {
    let unit = ring(&["a", "b", "c", "d"]);
    let mut doubled = Ring::new();
    for name in ["a", "b", "c", "d"] {
        doubled.insert(id(name), 2.0).unwrap();
    }
    assert!(Ownership::diff(&unit, &doubled, keys(1_000)).is_empty());
}

#[test]
fn changing_a_weight_moves_keys_toward_the_heavier_node_only() {
    let before = ring(&["a", "b", "c", "d"]);
    let mut after = before.clone();
    after.insert(id("b"), 2.0).unwrap();
    assert_eq!(after.len(), 4);
    assert_eq!(after.weight("b"), Some(2.0));
    let diff = Ownership::diff(&before, &after, keys(5_000));
    assert!(!diff.is_empty());
    assert!(diff
        .moves()
        .iter()
        .all(|m| m.to.as_ref().is_some_and(|to| to == "b")));
}

#[test]
fn owners_lists_successors_in_score_order() {
    let full = ring(&["a", "b", "c", "d", "e"]);
    for key in keys(100) {
        let top = full.owners(&key, 3);
        assert_eq!(top.len(), 3);
        assert_eq!(Some(top[0]), full.owner(&key));
        let mut distinct = top.clone();
        distinct.dedup();
        assert_eq!(distinct.len(), 3);
        // The runner-up is who takes over when the owner leaves.
        let mut without = full.clone();
        without.remove(top[0].as_str());
        assert_eq!(without.owner(&key), Some(top[1]));
    }
    assert_eq!(full.owners("k", 10).len(), 5);
}

#[test]
fn rejects_invalid_weights() {
    let mut ring = Ring::new();
    for bad in [0.0, -1.0, f64::NAN, f64::INFINITY, MAX_WEIGHT * 2.0] {
        assert!(matches!(
            ring.insert(id("a"), bad),
            Err(Error::InvalidWeight(_))
        ));
    }
    assert!(ring.is_empty());
}

#[test]
fn from_members_takes_live_members_with_their_weights() {
    let member = |name: &str, state: MemberState, weight: Option<&str>| Member {
        id: id(name),
        addr: String::new(),
        gossip_addr: "10.0.0.1:7946".parse().unwrap(),
        state,
        incarnation: 0,
        meta: weight
            .map(|w| Meta::from([("weight".to_owned(), w.to_owned())]))
            .unwrap_or_default(),
        since: Timestamp::EPOCH,
    };
    let members = [
        member("alive", MemberState::Alive, Some("3")),
        member("suspect", MemberState::Suspect, None),
        member("dead", MemberState::Dead, None),
        member("left", MemberState::Left, None),
    ];
    let ring = Ring::from_members(&members);
    let ids: Vec<&str> = ring.nodes().map(NodeId::as_str).collect();
    assert_eq!(ids, ["alive", "suspect"]);
    assert_eq!(ring.weight("alive"), Some(3.0));
    assert_eq!(ring.weight("suspect"), Some(1.0));
    assert!(ring.contains("suspect"));
    assert!(!ring.contains("dead"));
}

#[test]
fn ownership_diff_handles_empty_rings_and_no_keys() {
    let empty = Ring::new();
    let one = ring(&["a"]);
    let diff = Ownership::diff(&empty, &one, ["k1", "k2"]);
    assert_eq!(diff.moved(), 2);
    assert!(diff.moves().iter().all(|m| m.from.is_none()));
    let diff = Ownership::diff(&one, &empty, ["k1"]);
    assert_eq!(diff.lost_by("a").collect::<Vec<_>>(), ["k1"]);
    assert!(diff.moves()[0].to.is_none());
    let none = Ownership::diff(&one, &one, Vec::<String>::new());
    assert_eq!(none.total(), 0);
    assert_eq!(none.moved_fraction(), 0.0);
}
