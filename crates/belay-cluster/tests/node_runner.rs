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
