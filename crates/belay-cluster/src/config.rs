//! Identity and protocol tunables for one cluster node.
//!
//! The defaults ([`Config::new`]) are for nodes on one LAN or cloud region
//! and follow the values that have worked well for SWIM implementations in
//! practice: a 1 s protocol period, 500 ms probe timeout and 3 indirect
//! probers. [`Config::wan`] relaxes them for links with higher latency and
//! [`Config::local`] tightens them for processes on one machine (tests and
//! development), where round trips take microseconds.
//!
//! Several timings scale with the cluster size `n` (the number of live
//! members, including the local node); the formulas are on the methods
//! that compute them.

use std::net::SocketAddr;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::member::{validate_addr, validate_meta, Meta, NodeId};
use crate::wire::{cluster_tag, MAX_DATAGRAM, MIN_PACKET_SIZE};

/// Cluster names are at most this long.
pub const MAX_CLUSTER_NAME_LEN: usize = 64;

/// Membership configuration.
///
/// Fields are public so that configuration loaders can fill them directly;
/// [`validate`](Self::validate) (called by
/// [`Membership::new`](crate::Membership::new)) checks them together.
///
/// ```
/// use belay_cluster::{Config, NodeId};
/// use std::time::Duration;
///
/// let config = Config::new(NodeId::new("node-a").unwrap())
///     .with_api_addr("10.0.0.5:7070")
///     .with_seeds(["10.0.0.2:7946".parse().unwrap()])
///     .with_meta("zone", "eu-west-1a");
/// assert_eq!(config.probe_interval, Duration::from_secs(1));
/// assert_eq!(config.suspicion_timeout(10), Duration::from_secs(4));
/// config.validate().unwrap();
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// This node's identity; must be unique in the cluster.
    pub node_id: NodeId,
    /// Name shared by every node of one cluster. Packets from a cluster
    /// with another name are dropped, which keeps two clusters on one
    /// network apart when a seed list is wrong. It is not a secret and
    /// provides no security. Default `"belay"`.
    pub cluster_name: String,
    /// The API address other nodes should use for this node (see
    /// [`Member::addr`](crate::Member::addr)). Default empty.
    pub api_addr: String,
    /// The gossip address to advertise. `None` means "the transport's
    /// local address", which [`Node`](crate::Node) fills in; the pure state
    /// machine requires it to be set.
    pub advertise: Option<SocketAddr>,
    /// Gossip addresses of existing members to join through. Empty for the
    /// first node of a cluster.
    pub seeds: Vec<SocketAddr>,
    /// Metadata published with this node's `Alive` record.
    pub meta: Meta,
    /// The incarnation to start from. A node that restarts with the same id
    /// refutes whatever the cluster remembers about its previous run in any
    /// case; starting higher (e.g. from a persisted counter) just skips that
    /// round trip. Default 0.
    pub incarnation: u64,
    /// Protocol period: one member is probed per period. Default 1 s.
    pub probe_interval: Duration,
    /// How long to wait for a direct ack before asking other members to
    /// probe indirectly. Must be below `probe_interval`. Default 500 ms.
    pub probe_timeout: Duration,
    /// How many members are asked to probe a silent member on our behalf.
    /// Default 3.
    pub indirect_checks: usize,
    /// Scales the minimum suspicion timeout (see
    /// [`suspicion_timeout`](Self::suspicion_timeout)). Default 4.
    pub suspicion_mult: u32,
    /// The maximum suspicion timeout as a multiple of the minimum; a
    /// suspicion starts at the maximum and shrinks towards the minimum as
    /// other members independently confirm it. Default 6.
    pub suspicion_max_mult: u32,
    /// Scales how many times each update is retransmitted (see
    /// [`retransmit_limit`](Self::retransmit_limit)). Default 4.
    pub retransmit_mult: u32,
    /// Period of the dedicated gossip rounds that complement piggybacking.
    /// Default 200 ms.
    pub gossip_interval: Duration,
    /// Members sent to in each gossip round, at least 1. Gossip rounds are
    /// also what carries updates too large to piggyback on a probe.
    /// Default 3.
    pub gossip_fanout: usize,
    /// For how long members declared dead are still included in gossip
    /// rounds, so that a live node wrongly declared dead hears about it and
    /// refutes. Default 30 s.
    pub gossip_to_dead: Duration,
    /// Period of full-state anti-entropy exchanges with a random member
    /// (scaled for large clusters, see
    /// [`push_pull_period`](Self::push_pull_period)). Zero disables them.
    /// Default 30 s.
    pub push_pull_interval: Duration,
    /// Period of attempts to reach a random dead member (to heal
    /// partitions) and, while this node is alone, its seeds. Zero disables
    /// them. Default 10 s.
    pub reconnect_interval: Duration,
    /// Period of join attempts through the seeds until one answers.
    /// Default 1 s.
    pub join_retry_interval: Duration,
    /// How long [`Node::leave`](crate::Node::leave) keeps gossiping the
    /// departure before giving up. Default 5 s.
    pub leave_timeout: Duration,
    /// How long dead and departed members are remembered (and dead ones
    /// retried by reconnect attempts) before being forgotten. Default 24 h.
    pub tombstone_ttl: Duration,
    /// Largest datagram this node sends, in bytes. Piggybacked gossip and
    /// state transfers are packed up to it. The default of 1400 fits an
    /// Ethernet MTU with room for tunnel headers. Nodes accept datagrams of
    /// any size up to the UDP maximum, so the setting need not match across
    /// the cluster.
    pub max_packet_size: usize,
}
