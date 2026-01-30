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
