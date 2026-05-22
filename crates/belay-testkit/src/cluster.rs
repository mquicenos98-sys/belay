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
