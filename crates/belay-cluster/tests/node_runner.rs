//! The threaded `Node` runner in real time, over the in-memory network and
//! over UDP on loopback. Nodes use `Config::local` timings (100 ms protocol
//! period), so failures are detected within a couple of seconds.

use std::collections::BTreeSet;
use std::sync::mpsc::Receiver;
use std::thread;
use std::time::{Duration, Instant};

use belay_cluster::{
    Config, MemNetwork, MemberState, MembershipEvent, Node, NodeId, Transport, UdpTransport,
};
use belay_common::log::{set_max_level, LevelFilter};

fn quiet() {
    set_max_level(LevelFilter::Warn);
}

fn config(name: &str) -> Config {
    Config::local(NodeId::new(name).unwrap())
}

fn eventually(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Waits for an event matching `pred`, skipping others.
fn expect_event(
    events: &Receiver<MembershipEvent>,
    timeout: Duration,
    pred: impl Fn(&MembershipEvent) -> bool,
) -> MembershipEvent {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(left) {
            Ok(e) if pred(&e) => return e,
            Ok(_) => {}
            Err(e) => panic!("expected event did not arrive: {e}"),
        }
    }
}

fn live_ids(node: &Node) -> BTreeSet<String> {
    node.live_members()
        .iter()
        .map(|m| m.id.to_string())
        .collect()
}

fn start_cluster<T: Transport + 'static>(
    names: &[&str],
    mut transport: impl FnMut() -> T,
) -> Vec<Node> {
    let first = Node::start(config(names[0]), transport()).unwrap();
    let seed = first.gossip_addr();
    let mut nodes = vec![first];
    for name in &names[1..] {
        nodes.push(Node::start(config(name).with_seeds([seed]), transport()).unwrap());
    }
    let all: BTreeSet<String> = names.iter().map(|s| (*s).to_owned()).collect();
    assert!(
        eventually(Duration::from_secs(10), || nodes
            .iter()
            .all(|n| live_ids(n) == all)),
        "cluster of {} did not converge",
        names.len()
    );
    nodes
}

#[test]
fn nodes_form_a_cluster_and_agree_on_owners_and_leader() {
    quiet();
    let net = MemNetwork::new(1);
    let a = Node::start(config("a"), net.bind_next()).unwrap();
    let events = a.subscribe();
    let seed = a.gossip_addr();
    let others: Vec<Node> = ["b", "c", "d"]
        .iter()
        .map(|name| Node::start(config(name).with_seeds([seed]), net.bind_next()).unwrap())
        .collect();
    let mut joined = BTreeSet::new();
    while joined.len() < 3 {
        let e = expect_event(&events, Duration::from_secs(10), |e| {
            matches!(e, MembershipEvent::Joined(_))
        });
        joined.insert(e.member().id.to_string());
    }
    assert_eq!(joined, BTreeSet::from(["b".into(), "c".into(), "d".into()]));

    let mut nodes = vec![a];
    nodes.extend(others);
    assert!(eventually(Duration::from_secs(10), || nodes
        .iter()
        .all(|n| n.live_members().len() == 4)));
    for i in 0..200 {
        let key = format!("queue:{i}");
        let owners: BTreeSet<String> = nodes
            .iter()
            .map(|n| n.owner_of(&key).unwrap().id.to_string())
            .collect();
        assert_eq!(owners.len(), 1, "nodes disagree about {key}");
        let claimants = nodes.iter().filter(|n| n.is_owner(&key)).count();
        assert_eq!(claimants, 1, "{key} has {claimants} owners");
        let successors = nodes[0].owners(&key, 2);
        assert_eq!(successors.len(), 2);
        assert_ne!(successors[0].id, successors[1].id);
    }
    for n in &nodes {
        assert_eq!(n.leader().unwrap().id, "a");
        assert_eq!(n.is_leader(), n.id() == "a");
    }
}

#[test]
fn a_crashed_node_is_declared_failed_and_its_keys_move() {
    quiet();
    let net = MemNetwork::new(2);
    let nodes = start_cluster(&["a", "b", "c"], || net.bind_next());
    let events = nodes[0].subscribe();
    let keys: Vec<String> = (0..300).map(|i| format!("k{i}")).collect();
    let owned_by_c: Vec<&String> = keys
        .iter()
        .filter(|k| nodes[0].owner_of(k).unwrap().id == "c")
        .collect();
    assert!(!owned_by_c.is_empty());

    let bound = config("a").failure_detection_bound(3);
    nodes[2].shutdown();
    expect_event(
        &events,
        bound + Duration::from_secs(3),
        |e| matches!(e, MembershipEvent::Failed(m) if m.id == "c"),
    );
    assert_eq!(nodes[0].member("c").unwrap().state, MemberState::Dead);
    assert!(eventually(Duration::from_secs(5), || nodes[1]
        .member("c")
        .is_some_and(|m| m.state == MemberState::Dead)));
    for key in owned_by_c {
        let owner = nodes[0].owner_of(key).unwrap().id;
        assert!(owner == "a" || owner == "b");
        assert_eq!(nodes[1].owner_of(key).unwrap().id, owner);
    }
    assert!(!nodes[2].is_running());
    assert!(!nodes[2].is_owner("k1"));
}

#[test]
fn a_graceful_leave_propagates_as_left() {
    quiet();
    let net = MemNetwork::new(3);
    let nodes = start_cluster(&["a", "b", "c"], || net.bind_next());
    let events = nodes[0].subscribe();
    assert!(nodes[2].leave(), "leave did not finish spreading");
    assert!(!nodes[2].is_running());
    expect_event(
        &events,
        Duration::from_secs(5),
        |e| matches!(e, MembershipEvent::Left(m) if m.id == "c"),
    );
    assert!(eventually(Duration::from_secs(5), || nodes[1]
        .member("c")
        .is_some_and(|m| m.state == MemberState::Left)));
    // Well past the failure detection bound: still left, never failed.
    thread::sleep(Duration::from_millis(500));
    assert_eq!(nodes[0].member("c").unwrap().state, MemberState::Left);
    assert!(events
        .try_iter()
        .all(|e| !matches!(e, MembershipEvent::Failed(m) if m.id == "c")));
}

#[test]
fn a_node_restarted_after_being_declared_dead_rejoins() {
    quiet();
    let net = MemNetwork::new(4);
    let mut nodes = start_cluster(&["a", "b"], || net.bind_next());
    let b = nodes.pop().unwrap();
    let addr = b.gossip_addr();
    let before = b.local_member().incarnation;
    // Dropping the node stops it and frees its address, like a crash.
    drop(b);
    let bound = config("a").failure_detection_bound(2);
    assert!(eventually(bound + Duration::from_secs(3), || nodes[0]
        .member("b")
        .is_some_and(|m| m.state == MemberState::Dead)));
    let seed = nodes[0].gossip_addr();
    let b = Node::start(config("b").with_seeds([seed]), net.bind(addr).unwrap()).unwrap();
    assert!(b.wait_until_joined(Duration::from_secs(5)));
    assert!(eventually(Duration::from_secs(10), || {
        nodes[0]
            .member("b")
            .is_some_and(|m| m.state == MemberState::Alive && m.incarnation > before)
    }));
    assert_eq!(live_ids(&b).len(), 2);
}

#[test]
fn join_contacts_seeds_after_start() {
    quiet();
    let net = MemNetwork::new(5);
    let a = Node::start(config("a"), net.bind_next()).unwrap();
    let b = Node::start(config("b"), net.bind_next()).unwrap();
    assert_eq!(b.live_members().len(), 1);
    b.join(&[a.gossip_addr()]).unwrap();
    assert!(eventually(Duration::from_secs(5), || a
        .live_members()
        .len()
        == 2
        && b.live_members().len() == 2));
    b.shutdown();
    assert!(b.join(&[a.gossip_addr()]).is_err());
}

#[test]
fn nodes_over_udp_on_loopback_form_a_cluster_and_leave() {
    quiet();
    let nodes = start_cluster(&["u1", "u2", "u3"], || {
        UdpTransport::bind("127.0.0.1:0").unwrap()
    });
    for n in &nodes {
        assert_eq!(n.gossip_addr().ip().to_string(), "127.0.0.1");
        assert!(n.stats().packets_received > 0);
        assert_eq!(n.stats().packets_rejected, 0);
    }
    assert!(nodes[1].leave());
    assert!(eventually(Duration::from_secs(5), || {
        [&nodes[0], &nodes[2]]
            .iter()
            .all(|n| n.member("u2").is_some_and(|m| m.state == MemberState::Left))
    }));
}

#[test]
fn clusters_with_different_names_ignore_each_other() {
    quiet();
    let net = MemNetwork::new(6);
    let a = Node::start(config("a").with_cluster_name("blue"), net.bind_next()).unwrap();
    let b = Node::start(
        config("b")
            .with_cluster_name("green")
            .with_seeds([a.gossip_addr()]),
        net.bind_next(),
    )
    .unwrap();
    assert!(!b.wait_until_joined(Duration::from_millis(500)));
    assert_eq!(a.live_members().len(), 1);
    assert!(a.stats().packets_rejected > 0);
}
