//! [`TestCluster`] and its builder: harness servers gossiping over one
//! in-memory network, with membership and ownership helpers.

use std::collections::BTreeSet;
use std::fmt;
use std::time::Duration;

use belay_cluster::{MemNetwork, Transport};
use belay_common::Timestamp;
use belay_core::config::ClusterProfile;
use belay_core::{Config, Engine, Ownership, TickReport};
use belay_proto::api::MemberState;
use belay_proto::{QueueName, ScheduleId};

use crate::error::{Error, Result};
use crate::fixtures::{queue_name, start_time};
use crate::logs::init_test_logging;
use crate::server::{ClusterSeat, TestClock, TestServer};
use crate::wait::eventually;

type Configure = Box<dyn Fn(usize, &mut Config)>;

/// Builds a [`TestCluster`].
#[must_use]
pub struct TestClusterBuilder {
    size: usize,
    clock: TestClock,
    seed: u64,
    configure: Vec<Configure>,
}

impl TestClusterBuilder {
    /// Adjusts every node's configuration; `f` gets the node's index.
    pub fn configure(mut self, f: impl Fn(usize, &mut Config) + 'static) -> TestClusterBuilder {
        self.configure.push(Box::new(f));
        self
    }

    /// A manual clock starting at `start`, shared by every node.
    pub fn starting_at(mut self, start: Timestamp) -> TestClusterBuilder {
        self.clock = TestClock::Manual(belay_common::ManualClock::new(start));
        self
    }

    /// Real time with background threads on every node.
    pub fn system_clock(mut self) -> TestClusterBuilder {
        self.clock = TestClock::System;
        self
    }

    /// Seeds the gossip network's random decisions.
    pub fn seed(mut self, seed: u64) -> TestClusterBuilder {
        self.seed = seed;
        self
    }

    /// Starts every node. Does not wait for them to find each other; see
    /// [`TestCluster::wait_converged`].
    pub fn start(self) -> Result<TestCluster> {
        if self.size == 0 {
            return Err(Error::Invalid(
                "a cluster needs at least one node".to_owned(),
            ));
        }
        init_test_logging();
        let net = MemNetwork::new(self.seed);
        let mut nodes: Vec<TestServer> = Vec::with_capacity(self.size);
        for i in 0..self.size {
            let name = node_name(i);
            let seeds: Vec<String> = nodes
                .iter()
                .filter_map(TestServer::gossip_addr)
                .map(|a| a.to_string())
                .collect();
            let gossip = net.bind_next().local_addr();
            let node = TestServer::builder()
                .configure(|config| {
                    config.cluster.node_id = name;
                    config.cluster.profile = ClusterProfile::Local;
                    config.cluster.seeds = seeds;
                    for f in &self.configure {
                        f(i, config);
                    }
                })
                .clock(self.clock.clone())
                .seed(self.seed.wrapping_add(i as u64))
                .cluster_seat(ClusterSeat::new(net.clone(), gossip))
                .start()?;
            nodes.push(node);
        }
        Ok(TestCluster {
            net,
            clock: self.clock,
            nodes,
        })
    }
}

impl fmt::Debug for TestClusterBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestClusterBuilder")
            .field("size", &self.size)
            .field("clock", &self.clock)
            .field("seed", &self.seed)
            .finish_non_exhaustive()
    }
}

/// Several [`TestServer`]s gossiping over one in-memory network
/// ([`MemNetwork`]), each with its own HTTP server and store.
///
/// Membership runs in real time with the `local` timing profile (failures
/// are noticed within a second or two), while the engines share one
/// [`TestClock`], manual by default, so schedules fire only when the test
/// ticks. Node `i` is named `node-a`, `node-b`, ...; node 0 starts alone
/// and every later node joins through the ones before it.
///
/// [`kill`](TestCluster::kill) cuts a node off the network before stopping
/// it, so its goodbye never arrives and the others have to detect the
/// failure, as after a crash. Its store is kept, and
/// [`restart`](TestCluster::restart) brings it back with its jobs.
///
/// ```no_run
/// use belay_testkit::TestCluster;
///
/// let cluster = TestCluster::start(3)?; // waits until they agree
/// let queue = cluster.queue_owned_by(1, "emails")?;
/// assert_eq!(cluster.owner_of_queue(&queue), Some(1));
/// # Ok::<(), belay_testkit::Error>(())
/// ```
pub struct TestCluster {
    net: MemNetwork,
    clock: TestClock,
    nodes: Vec<TestServer>,
}

impl TestCluster {
    /// `size` nodes with every default, waited on until they agree on the
    /// membership.
    pub fn start(size: usize) -> Result<TestCluster> {
        let cluster = TestCluster::builder(size).start()?;
        cluster.wait_converged(Duration::from_secs(15))?;
        Ok(cluster)
    }

    /// A builder for `size` nodes.
    pub fn builder(size: usize) -> TestClusterBuilder {
        TestClusterBuilder {
            size,
            clock: TestClock::Manual(belay_common::ManualClock::new(start_time())),
            seed: 11,
            configure: Vec::new(),
        }
    }

    /// Number of nodes, dead or alive.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the cluster has no nodes (never, once started).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Node `i`.
    ///
    /// # Panics
    ///
    /// If `i` is out of range, like indexing.
    #[must_use]
    pub fn node(&self, i: usize) -> &TestServer {
        &self.nodes[i]
    }

    /// Node `i`'s id.
    #[must_use]
    pub fn node_id(&self, i: usize) -> &str {
        self.nodes[i].engine().node_id()
    }

    /// Indexes of the nodes that are running.
    #[must_use]
    pub fn live(&self) -> Vec<usize> {
        (0..self.nodes.len())
            .filter(|i| self.nodes[*i].is_running())
            .collect()
    }

    /// The shared clock.
    #[must_use]
    pub fn clock(&self) -> &TestClock {
        &self.clock
    }

    /// The gossip network, for faults beyond [`kill`](Self::kill).
    #[must_use]
    pub fn network(&self) -> &MemNetwork {
        &self.net
    }

    /// Whether every running node sees exactly the running nodes as alive
    /// and every stopped one as dead or gone. A merely suspected node still
    /// owns its keys, so suspicion does not count.
    #[must_use]
    pub fn is_converged(&self) -> bool {
        let live: BTreeSet<String> = self
            .live()
            .into_iter()
            .map(|i| self.node_id(i).to_owned())
            .collect();
        self.live().into_iter().all(|i| {
            let engine = self.nodes[i].engine();
            let members = engine.members();
            let alive: BTreeSet<&str> = members
                .iter()
                .filter(|m| m.state == MemberState::Alive)
                .map(|m| m.id.as_str())
                .collect();
            let settled = members.iter().all(|m| {
                live.contains(&m.id) || matches!(m.state, MemberState::Dead | MemberState::Left)
            });
            engine.is_joined()
                && settled
                && alive.len() == live.len()
                && live.iter().all(|id| alive.contains(id.as_str()))
        })
    }

    /// Waits until [`is_converged`](Self::is_converged).
    pub fn wait_converged(&self, timeout: Duration) -> Result<()> {
        eventually(timeout, || self.is_converged()).map_err(|_| {
            Error::timeout(
                format!(
                    "nodes {:?} to agree on the membership",
                    self.live()
                        .into_iter()
                        .map(|i| self.node_id(i))
                        .collect::<Vec<_>>()
                ),
                timeout,
            )
        })
    }

    /// The node every running node names as the owner of `queue`, if they
    /// agree.
    #[must_use]
    pub fn owner_of_queue(&self, queue: &QueueName) -> Option<usize> {
        self.agreed_owner(|engine| engine.owner_of(queue))
    }

    /// The node every running node names as the owner of schedule `id`,
    /// if they agree.
    #[must_use]
    pub fn owner_of_schedule(&self, id: &ScheduleId) -> Option<usize> {
        self.agreed_owner(|engine| engine.owner_of_schedule(id))
    }

    /// A queue named `<prefix>-<k>` that node `i` owns, by trying `k` =
    /// 0, 1, ... Rendezvous hashing spreads names evenly, so this is
    /// quick.
    pub fn queue_owned_by(&self, i: usize, prefix: &str) -> Result<QueueName> {
        for k in 0..1000 {
            let queue = queue_name(format!("{prefix}-{k}"))?;
            if self.owner_of_queue(&queue) == Some(i) {
                return Ok(queue);
            }
        }
        Err(Error::Invalid(format!(
            "no queue {prefix}-<k> is owned by node {i}; is it running?"
        )))
    }

    /// Crashes node `i`: cut off the network, then stopped, so the others
    /// must detect the failure. Its store is kept for
    /// [`restart`](Self::restart).
    pub fn kill(&mut self, i: usize) -> Result<()> {
        let node = self.running_node(i)?;
        if let Some(gossip) = node.gossip_addr() {
            self.net.isolate(gossip);
        }
        self.nodes[i].stop();
        Ok(())
    }

    /// Starts node `i` again on its store, joining through the running
    /// nodes. Heals the network first, which also lifts any partition
    /// set up through [`network`](Self::network).
    pub fn restart(&mut self, i: usize) -> Result<()> {
        if i >= self.nodes.len() {
            return Err(Error::Invalid(format!("no node {i}")));
        }
        self.nodes[i].stop();
        self.net.heal();
        let seeds: Vec<String> = self
            .live()
            .into_iter()
            .filter(|j| *j != i)
            .filter_map(|j| self.nodes[j].gossip_addr())
            .map(|a| a.to_string())
            .collect();
        self.nodes[i].set_seeds(seeds);
        self.nodes[i].restart()
    }

    /// Advances the manual clock by `d` and runs one round of duties on
    /// every running node; returns each node's report.
    pub fn tick(&self, d: Duration) -> Result<Vec<(usize, TickReport)>> {
        let now = self.clock.advance(d)?;
        Ok(self
            .live()
            .into_iter()
            .map(|i| (i, self.nodes[i].engine().tick(now)))
            .collect())
    }

    fn running_node(&self, i: usize) -> Result<&TestServer> {
        match self.nodes.get(i) {
            Some(node) if node.is_running() => Ok(node),
            Some(_) => Err(Error::Invalid(format!("node {i} is not running"))),
            None => Err(Error::Invalid(format!("no node {i}"))),
        }
    }

    fn agreed_owner(&self, owner: impl Fn(&Engine) -> Ownership) -> Option<usize> {
        let mut named = self.live().into_iter().map(|i| {
            let engine = self.nodes[i].engine();
            match owner(engine) {
                Ownership::Local => engine.node_id().to_owned(),
                Ownership::Remote(remote) => remote.node,
            }
        });
        let first = named.next()?;
        if !named.all(|n| n == first) {
            return None;
        }
        (0..self.nodes.len()).find(|i| self.node_id(*i) == first)
    }
}

impl fmt::Debug for TestCluster {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestCluster")
            .field("nodes", &self.nodes)
            .field("live", &self.live())
            .finish_non_exhaustive()
    }
}

/// `node-a` for 0, `node-b` for 1, ..., then `node-26`, `node-27`, ...
fn node_name(i: usize) -> String {
    match u8::try_from(i) {
        Ok(n) if n < 26 => format!("node-{}", char::from(b'a' + n)),
        _ => format!("node-{i}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nodes_are_named_by_letter_then_number() {
        assert_eq!(node_name(0), "node-a");
        assert_eq!(node_name(2), "node-c");
        assert_eq!(node_name(25), "node-z");
        assert_eq!(node_name(26), "node-26");
    }

    #[test]
    fn empty_clusters_are_refused() {
        assert!(matches!(
            TestCluster::builder(0).start(),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn two_nodes_converge_and_split_queues() {
        let cluster = TestCluster::start(2).unwrap();
        assert_eq!(cluster.live(), [0, 1]);
        let on_a = cluster.queue_owned_by(0, "q").unwrap();
        let on_b = cluster.queue_owned_by(1, "q").unwrap();
        assert_ne!(on_a, on_b);
        assert_eq!(cluster.node_id(1), "node-b");
        let ticks = cluster.tick(Duration::from_secs(1)).unwrap();
        assert_eq!(ticks.len(), 2);
    }
}
