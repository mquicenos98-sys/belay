//! Failure detection: one probe per protocol period.
//!
//! Each period the node pings the next member of a shuffled list (SWIM's
//! randomized round-robin, which bounds the time between probes of any one
//! member, unlike picking uniformly at random). Without an ack after
//! `probe_timeout`, it asks `indirect_checks` other members to ping the
//! target on its behalf, which separates "the target is down" from "the
//! path between us is lossy". If no ack, direct or relayed, arrives by the
//! end of the period, the target becomes suspect.

use std::net::SocketAddr;

use belay_common::{debug, Timestamp};

use super::{Membership, Outbound};
use crate::member::{MemberState, NodeId};
use crate::wire::{Message, Update};

/// Upper bound on pings relayed for others at once, so a flood of ping
/// requests cannot grow memory without limit.
const MAX_RELAYS: usize = 1024;

/// The probe of the current protocol period.
#[derive(Clone, Debug)]
pub(super) struct Probe {
    target: NodeId,
    addr: SocketAddr,
    /// The target's incarnation when the probe started; a failed probe
    /// suspects exactly this incarnation, so a refutation that arrived in
    /// the meantime wins.
    incarnation: u64,
    seq: u64,
    indirect_at: Timestamp,
    ends_at: Timestamp,
    acked: bool,
    indirect_sent: bool,
}

impl Probe {
    pub(super) fn next_deadline(&self) -> Timestamp {
        if self.acked || self.indirect_sent {
            self.ends_at
        } else {
            self.indirect_at.min(self.ends_at)
        }
    }
}

/// A ping sent on behalf of another member's ping request.
#[derive(Clone, Copy, Debug)]
pub(super) struct Relay {
    /// Where to forward the ack.
    reply_to: SocketAddr,
    /// The requester's seq, echoed in the forwarded ack.
    seq: u64,
    pub(super) expires: Timestamp,
}
