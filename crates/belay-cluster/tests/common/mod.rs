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

impl Sim {
    /// `n` nodes started together, all joining through node 0.
    pub fn new(seed: u64, n: usize) -> Sim {
        Sim::with_config(seed, n, |_, c| c)
    }

    /// Like [`new`](Self::new), with a hook to adjust each node's config.
    pub fn with_config(seed: u64, n: usize, adjust: impl Fn(usize, Config) -> Config) -> Sim {
        Sim::build(seed, n, |i| (i > 0).then_some(0), adjust)
    }

    /// `n` nodes where node `i` joins through node `seed_of(i)`, if any.
    pub fn with_topology(seed: u64, n: usize, seed_of: impl Fn(usize) -> Option<usize>) -> Sim {
        Sim::build(seed, n, seed_of, |_, c| c)
    }

    fn build(
        seed: u64,
        n: usize,
        seed_of: impl Fn(usize) -> Option<usize>,
        adjust: impl Fn(usize, Config) -> Config,
    ) -> Sim {
        quiet();
        let clock = ManualClock::new(Timestamp::from_secs(1_750_000_000));
        let net = MemNetwork::with_clock(seed, clock.shared());
        net.set_latency(Duration::from_millis(1), Duration::from_millis(4));
        let mut sim = Sim {
            clock,
            net,
            nodes: Vec::new(),
            seed,
            restarts: 0,
        };
        for i in 0..n {
            let seeds: Vec<SocketAddr> =
                seed_of(i).map(|s| sim.nodes[s].addr).into_iter().collect();
            sim.add_node_with(|id, addr| adjust(i, config(id, addr).with_seeds(seeds)));
        }
        sim
    }

    /// Makes node `i` contact node `via` with a full state exchange.
    pub fn join(&mut self, i: usize, via: usize) {
        let now = self.now();
        let seed = self.nodes[via].addr;
        let out = self.nodes[i].membership.join(&[seed], now);
        self.send(i, out);
    }

    pub fn now(&self) -> Timestamp {
        self.clock.now()
    }

    /// Starts one more node that joins through node 0; returns its index.
    pub fn add_node(&mut self) -> usize {
        let seed = self.nodes[0].addr;
        self.add_node_with(|id, addr| config(id, addr).with_seeds([seed]))
    }

    pub fn add_node_with(&mut self, make: impl FnOnce(&NodeId, SocketAddr) -> Config) -> usize {
        let i = self.nodes.len();
        let id = NodeId::new(name(i)).unwrap();
        let transport = self.net.bind_next();
        let addr = transport.local_addr();
        let rng = Rng::seed_from_u64(self.seed ^ (i as u64 + 1).wrapping_mul(0x9E37_79B9));
        let membership = Membership::new(make(&id, addr), rng, self.now()).unwrap();
        self.nodes.push(SimNode {
            id,
            addr,
            transport: Some(transport),
            membership,
            events: Vec::new(),
        });
        i
    }

    fn send(&self, i: usize, out: Vec<Outbound>) {
        if let Some(transport) = &self.nodes[i].transport {
            for o in out {
                transport.send_to(o.to, &o.bytes).unwrap();
            }
        }
    }

    /// Advances to the next event and processes everything due then.
    pub fn step(&mut self) {
        let now = self.now();
        let mut next = self.net.next_delivery().unwrap_or(Timestamp::MAX);
        for node in self.nodes.iter().filter(|n| n.is_running()) {
            next = next.min(node.membership.next_wakeup());
        }
        let next = next.max(now + Duration::from_millis(1));
        self.clock.set(next);
        self.process(next);
    }

    fn process(&mut self, now: Timestamp) {
        for i in 0..self.nodes.len() {
            loop {
                let Some(datagram) = self.nodes[i].transport.as_ref().and_then(|t| t.try_recv())
                else {
                    break;
                };
                let out =
                    self.nodes[i]
                        .membership
                        .handle_datagram(datagram.from, &datagram.bytes, now);
                self.send(i, out);
            }
        }
        for i in 0..self.nodes.len() {
            let node = &mut self.nodes[i];
            if !node.is_running() || node.membership.next_wakeup() > now {
                continue;
            }
            let out = node.membership.tick(now);
            self.send(i, out);
        }
        for node in &mut self.nodes {
            node.events
                .extend(node.membership.drain_events().into_iter().map(|e| (now, e)));
            if node.membership.state() == LocalState::Left {
                node.transport = None;
            }
        }
    }

    pub fn run_for(&mut self, d: Duration) {
        let until = self.now() + d;
        while self.now() < until {
            self.step();
        }
    }

    /// Steps until `done` holds, for at most `limit`; returns the time it
    /// took, or `None` if it never held.
    pub fn run_until(
        &mut self,
        limit: Duration,
        mut done: impl FnMut(&Sim) -> bool,
    ) -> Option<Duration> {
        let start = self.now();
        let until = start + limit;
        loop {
            if done(self) {
                return Some(self.now().saturating_duration_since(start));
            }
            if self.now() >= until {
                return None;
            }
            self.step();
        }
    }

    /// Crashes node `i`: it stops processing and its address goes dark.
    pub fn kill(&mut self, i: usize) {
        self.nodes[i].transport = None;
    }

    /// Restarts crashed node `i` with the same id and address, a fresh
    /// state machine, the given incarnation, and node `seed` as its seed.
    pub fn restart(&mut self, i: usize, incarnation: u64, seed: usize) {
        assert!(!self.nodes[i].is_running(), "node {i} is still running");
        self.restarts += 1;
        let id = self.nodes[i].id.clone();
        let addr = self.nodes[i].addr;
        let transport = self.net.bind(addr).unwrap();
        let config = config(&id, addr)
            .with_seeds([self.nodes[seed].addr])
            .with_incarnation(incarnation);
        let rng = Rng::seed_from_u64(self.seed ^ self.restarts.wrapping_mul(0xA24B_AED4));
        self.nodes[i].membership = Membership::new(config, rng, self.now()).unwrap();
        self.nodes[i].transport = Some(transport);
    }

    /// Starts a graceful departure of node `i`.
    pub fn leave(&mut self, i: usize) {
        let now = self.now();
        let out = self.nodes[i].membership.leave(now);
        self.send(i, out);
    }

    pub fn running(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.nodes.len()).filter(|&i| self.nodes[i].is_running())
    }

    /// How node `observer` sees node `subject`.
    pub fn state(&self, observer: usize, subject: usize) -> Option<MemberState> {
        self.nodes[observer]
            .membership
            .member(self.nodes[subject].id.as_str())
            .map(|m| m.state)
    }

    pub fn incarnation(&self, observer: usize, subject: usize) -> Option<u64> {
        self.nodes[observer]
            .membership
            .member(self.nodes[subject].id.as_str())
            .map(|m| m.incarnation)
    }

    pub fn live_set(&self, i: usize) -> BTreeSet<String> {
        self.nodes[i]
            .membership
            .live_members()
            .map(|m| m.id.to_string())
            .collect()
    }

    /// Whether every running node sees exactly the running nodes as live.
    pub fn converged(&self) -> bool {
        let expected: BTreeSet<String> = self
            .running()
            .filter(|&i| self.nodes[i].membership.state() == LocalState::Active)
            .map(|i| self.nodes[i].id.to_string())
            .collect();
        self.running().all(|i| self.live_set(i) == expected)
    }

    /// Whether every running node other than `subject` sees it in `state`.
    pub fn all_see(&self, subject: usize, state: MemberState) -> bool {
        self.running()
            .filter(|&i| i != subject)
            .all(|i| self.state(i, subject) == Some(state))
    }

    pub fn ring(&self, i: usize) -> Ring {
        self.nodes[i].membership.ring()
    }

    /// Owners of `keys` as node `i` sees them.
    pub fn owners(&self, i: usize, keys: &[String]) -> BTreeMap<String, NodeId> {
        let ring = self.ring(i);
        keys.iter()
            .map(|k| {
                (
                    k.clone(),
                    ring.owner(k).cloned().expect("ring is not empty"),
                )
            })
            .collect()
    }

    /// Events about node `subject` seen by any node, as (observer, event).
    pub fn events_about(&self, subject: usize) -> Vec<(usize, &MembershipEvent)> {
        let id = &self.nodes[subject].id;
        self.nodes
            .iter()
            .enumerate()
            .flat_map(|(i, n)| n.events.iter().map(move |(_, e)| (i, e)))
            .filter(|(_, e)| e.member().id == *id)
            .collect()
    }

    pub fn failures(&self) -> usize {
        self.nodes
            .iter()
            .flat_map(|n| &n.events)
            .filter(|(_, e)| matches!(e, MembershipEvent::Failed(_)))
            .count()
    }

    pub fn total_stats(&self) -> (u64, u64) {
        self.nodes.iter().fold((0, 0), |(s, r), n| {
            let stats = n.membership.stats();
            (s + stats.suspicions, r + stats.refutations)
        })
    }
}

pub fn keys(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("queue:{i}")).collect()
}
