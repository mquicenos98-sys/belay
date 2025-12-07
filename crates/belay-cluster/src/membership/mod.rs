//! The SWIM membership state machine.
//!
//! [`Membership`] does no I/O and reads no clock: the caller feeds it
//! received datagrams ([`handle`](Membership::handle) /
//! [`handle_datagram`](Membership::handle_datagram)) and calls
//! [`tick`](Membership::tick) at least by [`next_wakeup`](Membership::next_wakeup),
//! passing the current time each time, and sends the [`Outbound`] datagrams
//! each call returns. Randomness comes from the [`Rng`] it was built with.
//! Given the same seed, inputs and times it produces the same outputs,
//! which is what makes whole-cluster simulations reproducible.
//!
//! The work is split by concern: `probe` (failure detection), `merge`
//! (applying updates, suspicion, refutation) and `sync` (join,
//! anti-entropy, reconnection).

mod merge;

mod probe;

mod sync;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

use belay_common::{debug, info, warn, Rng, Timestamp};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::event::MembershipEvent;
use crate::gossip::GossipQueue;
use crate::member::{validate_meta, Member, MemberState, Meta, NodeId};
use crate::ring::Ring;
use crate::suspicion::Suspicion;
use crate::wire::{self, FrameWriter, Message, Packet, Update};

use self::probe::{Probe, Relay};

/// A datagram to send.
#[derive(Clone, PartialEq, Eq)]
pub struct Outbound {
    /// Destination gossip address.
    pub to: SocketAddr,
    /// The encoded datagram, at most
    /// [`max_packet_size`](Config::max_packet_size) bytes.
    pub bytes: Vec<u8>,
}

impl fmt::Debug for Outbound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Outbound")
            .field("to", &self.to)
            .field("len", &self.bytes.len())
            .finish()
    }
}

/// The local node's own progress through the cluster lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LocalState {
    /// Seeds are configured and none has answered yet.
    Joining,
    /// Part of the cluster (or the first node of one).
    Active,
    /// [`leave`](Membership::leave) was called; still spreading the news.
    Leaving,
    /// Gone: the state machine ignores all input.
    Left,
}

impl LocalState {
    /// Lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            LocalState::Joining => "joining",
            LocalState::Active => "active",
            LocalState::Leaving => "leaving",
            LocalState::Left => "left",
        }
    }
}

impl fmt::Display for LocalState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// Counters describing the protocol's activity since start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MembershipStats {
    /// Datagrams accepted.
    pub packets_received: u64,
    /// Datagrams dropped as malformed, from another cluster or version.
    pub packets_rejected: u64,
    /// Datagrams produced.
    pub packets_sent: u64,
    /// Probes started (one per protocol period with someone to probe).
    pub probes: u64,
    /// Probes that fell back to indirect probing.
    pub indirect_probes: u64,
    /// Probes that got no ack at all within their period.
    pub probe_failures: u64,
    /// Members this node moved to suspect (on its own or through gossip).
    pub suspicions: u64,
    /// Times this node refuted a claim that it was suspect, dead or gone.
    pub refutations: u64,
    /// Full-state exchanges this node initiated.
    pub syncs_sent: u64,
    /// Full-state datagrams received.
    pub syncs_received: u64,
}

/// Upcoming deadlines of the periodic duties.
#[derive(Clone, Copy, Debug)]
struct Timers {
    probe: Timestamp,
    gossip: Timestamp,
    push_pull: Timestamp,
    reconnect: Timestamp,
    join: Timestamp,
    reap: Timestamp,
}

/// How often tombstones are checked for expiry.
const REAP_CHECK: Duration = Duration::from_secs(1);

/// How often a node using this node's id is reported.
const CONFLICT_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// One node's view of the cluster, maintained by the SWIM protocol.
///
/// See the [crate documentation](crate) for the protocol.
///
/// ```
/// use belay_cluster::{Config, Membership, NodeId};
/// use belay_common::{Rng, Timestamp};
///
/// let t0 = Timestamp::from_secs(1_000);
/// let config = |id: &str, port: u16| {
///     Config::new(NodeId::new(id).unwrap())
///         .with_advertise(format!("10.0.0.1:{port}").parse().unwrap())
/// };
/// let mut a = Membership::new(config("a", 7001), Rng::seed_from_u64(1), t0).unwrap();
/// let mut b = Membership::new(
///     config("b", 7002).with_seeds(["10.0.0.1:7001".parse().unwrap()]),
///     Rng::seed_from_u64(2),
///     t0,
/// )
/// .unwrap();
///
/// // Deliver datagrams by hand until both sides are quiet.
/// let a_addr = a.local_member().gossip_addr;
/// let b_addr = b.local_member().gossip_addr;
/// let mut to_a = Vec::new();
/// let mut to_b = Vec::new();
/// for out in b.tick(t0) {
///     assert_eq!(out.to, a_addr);
///     to_a.push(out.bytes);
/// }
/// while !to_a.is_empty() || !to_b.is_empty() {
///     for bytes in to_a.drain(..).collect::<Vec<_>>() {
///         to_b.extend(a.handle_datagram(b_addr, &bytes, t0).into_iter().map(|o| o.bytes));
///     }
///     for bytes in to_b.drain(..).collect::<Vec<_>>() {
///         to_a.extend(b.handle_datagram(a_addr, &bytes, t0).into_iter().map(|o| o.bytes));
///     }
/// }
/// assert_eq!(a.live_count(), 2);
/// assert_eq!(b.live_count(), 2);
/// assert_eq!(a.leader().unwrap().id, "a");
/// ```
pub struct Membership {
    config: Config,
    cluster: u32,
    local: NodeId,
    /// Every known member, the local node included, in id order.
    members: BTreeMap<NodeId, Member>,
    state: LocalState,
    rng: Rng,
    seq: u64,
    probe: Option<Probe>,
    /// Randomized round-robin order for probing; reshuffled every round.
    probe_list: Vec<NodeId>,
    probe_index: usize,
    /// Pings sent on behalf of a ping request, by our seq.
    relays: BTreeMap<u64, Relay>,
    suspicions: BTreeMap<NodeId, Suspicion>,
    gossip: GossipQueue,
    seeds: Vec<SocketAddr>,
    timers: Timers,
    /// When each dead member that contacted us was last told it is dead.
    dead_notices: HashMap<NodeId, Timestamp>,
    conflict_logged_at: Option<Timestamp>,
    events: Vec<MembershipEvent>,
    version: u64,
    stats: MembershipStats,
}

impl fmt::Debug for Membership {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Membership")
            .field("local", &self.local)
            .field("state", &self.state)
            .field("members", &self.members.len())
            .field("live", &self.live_count())
            .field("pending_updates", &self.gossip.len())
            .finish_non_exhaustive()
    }
}
