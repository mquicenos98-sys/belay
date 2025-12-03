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
