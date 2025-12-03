//! Cluster membership and key ownership for belay.
//!
//! A belay cluster is a set of nodes that find each other through a few
//! seed addresses, keep track of who is alive with the SWIM protocol, and
//! split ownership of keys (queue names, schedule ids) between the live
//! nodes by rendezvous hashing. Every node computes ownership locally from
//! its own view of the membership; once views agree, so do owners.
//!
//! ```
//! use belay_cluster::{Config, MemNetwork, Node, NodeId};
//! use std::time::Duration;
//!
//! let net = MemNetwork::new(7); // or UdpTransport::bind("0.0.0.0:7946")
//! let first = Node::start(Config::local(NodeId::new("node-1").unwrap()), net.bind_next())?;
//! let second = Node::start(
//!     Config::local(NodeId::new("node-2").unwrap()).with_seeds([first.gossip_addr()]),
//!     net.bind_next(),
//! )?;
//! assert!(second.wait_until_joined(Duration::from_secs(5)));
//!
//! let events = first.subscribe();
//! let owner = second.owner_of("queue:emails").expect("the cluster is not empty");
//! if second.is_owner("queue:emails") {
//!     // serve the queue here
//! } else {
//!     // redirect to owner.addr
//! }
//! # let _ = (events, owner);
//! # Ok::<(), belay_cluster::Error>(())
//! ```
//!
//! # Layers
//!
//! | layer | type | role |
//! |---|---|---|
//! | identity | [`NodeId`], [`Member`], [`MemberState`] | who is in the cluster, and in what state |
//! | protocol | [`Membership`] | the SWIM state machine: pure, tick-driven, deterministic |
//! | wire | [`wire`] | versioned, checksummed, size-bounded datagrams |
//! | placement | [`Ring`], [`Ownership`] | rendezvous hashing over live members |
//! | transport | [`Transport`], [`UdpTransport`], [`MemNetwork`] | best-effort datagrams |
//! | runner | [`Node`] | threads driving the protocol, snapshots for readers, events |
//!
//! [`Membership`] performs no I/O and reads no clock. It is fed received
//! datagrams and the current time, and returns datagrams to send; its
//! randomness comes from a seeded [`Rng`](belay_common::Rng). A whole
//! cluster can therefore be simulated in virtual time on one thread over a
//! [`MemNetwork`] with a [`ManualClock`](belay_common::ManualClock), and
//! every run replays exactly from its seed. The crate's own tests do this
//! for crashes, packet loss, partitions and restarts.
//!
//! # The protocol
//!
//! The protocol is SWIM (Das, Gupta and Motivala, 2002) with the
//! suspicion mechanism from the same paper, dynamic suspicion timeouts
//! from Lifeguard (Dadgar et al., 2018), and full-state push-pull
//! exchanges for joining and anti-entropy, in the style of HashiCorp's
//! memberlist. Durations below are [`Config`] fields.
//!
//! **Failure detection.** Every `probe_interval` (the protocol period)
//! each node sends a `Ping` to one member, taken from a list that is
//! shuffled at the start of each round, so every member is probed once
//! per round. Without an `Ack` within `probe_timeout`, the node sends
//! `PingReq` to `indirect_checks` other members, which ping the target and
//! relay its ack. If no ack, direct or relayed, arrives by the end of the
//! period, the target becomes **suspect**.
//!
//! **Suspicion.** A suspect is still a member: it is probed, it keeps its
//! keys. The suspicion is gossiped (and sent to the suspect directly). A
//! suspect that hears about it **refutes** by incrementing its
//! *incarnation* number and gossiping `Alive` with the new incarnation,
//! which overrides the suspicion everywhere. Only a node increments its
//! own incarnation. A suspicion that is not refuted within the suspicion
//! timeout turns into **dead**, which is gossiped too. The timeout starts
//! at `suspicion_max_mult` times the minimum and shrinks towards the
//! minimum, `suspicion_mult * max(1, log10(n)) * probe_interval`, as other
//! members independently suspect the same incarnation: a real crash is
//! noticed by everyone and declared quickly, while one node with a bad
//! link has a hard time getting a healthy member killed.
//!
//! **Gossip.** Membership updates (`Alive`, `Suspect`, `Dead`, `Left`)
//! are piggybacked on every datagram, in the space the message leaves
//! free, and additionally sent every `gossip_interval` to `gossip_fanout`
//! random members (including members declared dead less than
//! `gossip_to_dead` ago, so that a node wrongly declared dead finds out).
//! Each update is sent at most `retransmit_mult * ceil(log10(n + 1))`
//! times, least-sent first; a newer update about a member replaces the
//! older one in the queue.
//!
//! **Joining.** A node with seeds sends each of them a `Sync` carrying its
//! own record and asks for theirs, every `join_retry_interval` until one
//! answers. The seed merges the newcomer (which it then gossips) and
//! answers with its full view, split over as many datagrams as needed.
//!
//! **Anti-entropy.** Every `push_pull_interval` (scaled up beyond 32
//! members) a node swaps full views with a random member, repairing
//! anything gossip missed. Every `reconnect_interval` it also sends its
//! view to a random member it believes dead, and to its seeds while it has
//! no live peer at all: after a partition heals this is how the two sides
//! find each other again, each side refuting the other's belief that it
//! died. Dead records received this way are merged as suspicions, so a
//! stale report cannot kill a live member without giving it the chance to
//! refute.
//!
//! **Leaving.** [`Node::leave`] gossips `Left` until the update has used
//! up its retransmissions, then stops. Departed members are reported as
//! [`MemberState::Left`], never as dead, and are not probed.
//!
//! **Tombstones.** Dead and departed members are remembered for
//! `tombstone_ttl`, then forgotten ([`MembershipEvent::Reaped`]).
//!
//! **Restarts.** A node restarting with the same id (at any incarnation)
//! hears what the cluster remembers about it (alive, dead or left) in the
//! join answer, and refutes it with a higher incarnation; starting with a
//! higher [`Config::incarnation`] skips that step.
//!
//! # Wire format
//!
//! Each datagram is a 9-byte header (protocol version, CRC-32C of the
//! rest, a tag derived from [`Config::cluster_name`]), the sender's id, one
//! message (`Ping`, `Ack`, `PingReq`, `Sync` or `Gossip`) and the
//! piggybacked updates; see [`wire`] for the byte layout. Datagrams from
//! another protocol version or cluster, or failing the checksum or any
//! validation, are dropped and counted
//! ([`MembershipStats::packets_rejected`]). Nodes send datagrams of at
//! most [`Config::max_packet_size`] bytes (1400 by default) and accept any
//! size up to the UDP maximum.
//!
//! # Timing
//!
//! | setting | default | effect |
//! |---|---|---|
//! | `probe_interval` | 1 s | protocol period: one probe per node per period |
//! | `probe_timeout` | 500 ms | wait for a direct ack before probing indirectly |
//! | `indirect_checks` | 3 | members asked to probe indirectly |
//! | `suspicion_mult` | 4 | minimum suspicion timeout: 4 s up to 10 members, 8 s at 100 |
//! | `suspicion_max_mult` | 6 | maximum suspicion timeout, as a multiple of the minimum |
//! | `retransmit_mult` | 4 | sends per update: 4 up to 9 members, 8 up to 99 |
//! | `gossip_interval` / `gossip_fanout` | 200 ms / 3 | extra gossip rounds |
//! | `push_pull_interval` | 30 s | full-state anti-entropy |
//! | `reconnect_interval` | 10 s | attempts to reach dead members and seeds |
//! | `tombstone_ttl` | 24 h | how long dead and departed members are remembered |
//!
//! [`Config::failure_detection_bound`] gives an upper bound, absent message
//! loss, on the time until every member has declared a crashed member
//! dead. With the defaults in a 10-node cluster it is about 45 s; typical
//! detection takes 5 to 10 s, because the first of nine probers notices
//! within a few periods and confirmations shrink the suspicion timeout.
//!
//! # Guarantees
//!
//! - **Completeness**: a crashed member is eventually declared dead by
//!   every live member (within the bound above if messages are not lost).
//! - **Accuracy under loss**: a member that keeps answering probes through
//!   at least one path is not declared dead; the indirect probes and the
//!   refutation window make false positives rare even with substantial
//!   packet loss (the tests run at 10% loss without any).
//! - **Convergence**: once the network is connected and quiet, all views
//!   become identical, and with them the rendezvous owners of every key
//!   and the [`leader`](Node::leader).
//! - **Minimal movement**: when a member joins, only the keys it now owns
//!   move (about `1/n` of them); when one leaves or dies, only its keys
//!   move, spread evenly over the others.
//! - **Clock skew tolerance**: nodes never compare timestamps with each
//!   other; every timeout is a local duration.
//!
//! # What is not provided
//!
//! - **No consensus.** Views converge but are not agreed on at any instant.
//!   While views differ (for a few gossip rounds after every change, or for
//!   the whole duration of a network partition), two nodes may both
//!   believe they own a key, or both believe they are the leader. Anything
//!   built on ownership must tolerate a key being processed by two nodes
//!   for a while, or add its own fencing.
//! - **No replication.** Ownership decides where a queue is served, not
//!   where its data lives: a node's queues are unavailable while it is
//!   down, and moving a queue to a new owner moves no data.
//! - **No security.** Gossip is neither authenticated nor encrypted. The
//!   cluster name keeps misconfigured clusters apart but is not a secret;
//!   run gossip on a trusted network.
//! - **No TCP fallback.** Full-state exchanges use several datagrams
//!   instead of a TCP connection; a lost datagram costs only the records
//!   in it, which the next exchange repairs.
//! - **No Lifeguard local health multiplier.** A node that is itself
//!   overloaded or partially cut off does not slow its own probing down,
//!   so it can raise suspicions against healthy members. The dynamic
//!   suspicion timeout limits the damage, since one node's suspicion alone
//!   runs for the maximum timeout.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod config;
mod error;
mod event;
mod gossip;
mod member;
mod membership;
mod node;
mod ring;
mod suspicion;
mod transport;
pub mod wire;

pub use config::{Config, MAX_CLUSTER_NAME_LEN};
pub use error::{Error, Result};
pub use event::MembershipEvent;
pub use member::{
    validate_addr, validate_meta, Member, MemberState, Meta, NodeId, MAX_ADDR_LEN, MAX_META_BYTES,
    MAX_META_ENTRIES, MAX_META_KEY_LEN, MAX_NODE_ID_LEN, MAX_WEIGHT, WEIGHT_KEY,
};
pub use membership::{LocalState, Membership, MembershipStats, Outbound};
pub use node::Node;
pub use ring::{Move, Ownership, Ring};
pub use transport::{
    Datagram, Faults, MemNetwork, MemTransport, NetStats, Transport, UdpTransport,
};
