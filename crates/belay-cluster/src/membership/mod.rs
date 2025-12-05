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
