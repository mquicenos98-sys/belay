//! Deterministic whole-cluster simulation.
//!
//! Every node is a bare `Membership` state machine on a `MemNetwork`
//! whose clock is a `ManualClock`. The simulation jumps the clock from
//! event to event (the earliest datagram delivery or node deadline),
//! delivers what is due, ticks the nodes that are due, and sends what they
//! produce. It runs on the test thread, so a seed fixes the whole run.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Once;
use std::time::Duration;

use belay_cluster::{
    Config, LocalState, MemNetwork, MemTransport, MemberState, Membership, MembershipEvent, NodeId,
    Outbound, Ring, Transport,
};
use belay_common::log::{set_max_level, LevelFilter};
use belay_common::{Clock, ManualClock, Rng, Timestamp};

/// Silences info-level logs from the nodes (they all run on the test
/// thread, so tests that want to see logs can still capture them).
pub fn quiet() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| set_max_level(LevelFilter::Warn));
}

pub fn name(i: usize) -> String {
    format!("node-{i:02}")
}

pub struct SimNode {
    pub id: NodeId,
    pub addr: SocketAddr,
    /// `None` once the node has crashed or finished leaving.
    pub transport: Option<MemTransport>,
    pub membership: Membership,
    /// Every event this node produced, with the time it happened.
    pub events: Vec<(Timestamp, MembershipEvent)>,
}

impl SimNode {
    pub fn is_running(&self) -> bool {
        self.transport.is_some()
    }
}

pub struct Sim {
    pub clock: ManualClock,
    pub net: MemNetwork,
    pub nodes: Vec<SimNode>,
    seed: u64,
    restarts: u64,
}

/// Every node is configured with the defaults, as in production.
pub fn config(id: &NodeId, addr: SocketAddr) -> Config {
    Config::new(id.clone()).with_advertise(addr)
}
