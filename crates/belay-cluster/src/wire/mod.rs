//! The datagram format.
//!
//! Every datagram is one [`Packet`]: a fixed header, the sender's id, one
//! primary [`Message`] and zero or more piggybacked gossip [`Update`]s.
//!
//! ```text
//! offset  size  field
//!      0     1  protocol version (1)
//!      1     4  CRC-32C (Castagnoli) of bytes 5.., little-endian
//!      5     4  cluster tag: hash of the cluster name, little-endian
//!      9     *  sender node id (varint length + bytes)
//!           1   message kind, then the message fields
//!           *   update count (varint), then the updates
//! ```
//!
//! Integers are unsigned LEB128 varints unless noted; strings carry a
//! varint length. Socket addresses are a family byte (4 or 6), the IP
//! octets and a little-endian `u16` port.
//!
//! | kind | message | fields |
//! |---|---|---|
//! | 1 | `Ping` | seq, target id |
//! | 2 | `Ack` | seq |
//! | 3 | `PingReq` | seq, target id, target address |
//! | 4 | `Sync` | flags (bit 0: reply wanted), member records |
//! | 5 | `Gossip` | (none: the updates are the payload) |
//!
//! | tag | update | fields |
//! |---|---|---|
//! | 1 | `Alive` | id, incarnation, API address, gossip address, metadata |
//! | 2 | `Suspect` | id, incarnation, suspecting node |
//! | 3 | `Dead` | id, incarnation, declaring node |
//! | 4 | `Left` | id, incarnation |
//!
//! Decoding is strict: unknown kinds, unknown flag bits, invalid ids or
//! metadata, over-long varints and trailing bytes are all rejected, and
//! element counts are checked against the remaining input before anything
//! is allocated. A datagram that fails any check is dropped whole.

mod fields;

use std::fmt;
use std::net::SocketAddr;

use belay_common::codec::{DecodeError, Decoder, Encoder};
use belay_common::hash::{crc32c, fnv1a64};

use crate::member::{str_encoded_len, Member, MemberState, Meta, NodeId};

/// The protocol version written in, and required of, every datagram.
pub const PROTOCOL_VERSION: u8 = 1;

/// Size of the fixed header (version, checksum, cluster tag).
pub const HEADER_LEN: usize = 9;

/// Largest UDP payload over IPv4; larger datagrams are rejected.
pub const MAX_DATAGRAM: usize = 65_507;

/// Smallest allowed [`Config::max_packet_size`](crate::Config::max_packet_size).
/// The limits on ids, addresses and metadata guarantee that any single
/// update or member record fits in a packet of this size.
pub const MIN_PACKET_SIZE: usize = 1_200;

/// Space kept free for the update count while packing updates.
const COUNT_RESERVE: usize = 3;

const MSG_PING: u8 = 1;

const MSG_ACK: u8 = 2;

const MSG_PING_REQ: u8 = 3;

const MSG_SYNC: u8 = 4;

const MSG_GOSSIP: u8 = 5;

const SYNC_REPLY: u8 = 0b1;

/// The tag identifying a cluster in packet headers: FNV-1a of the name,
/// folded to 32 bits.
#[must_use]
pub fn cluster_tag(cluster_name: &str) -> u32 {
    let h = fnv1a64(cluster_name.as_bytes());
    (h ^ (h >> 32)) as u32
}

/// One datagram's contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    /// The node that sent the datagram.
    pub sender: NodeId,
    /// The primary message.
    pub message: Message,
    /// Piggybacked membership updates.
    pub updates: Vec<Update>,
}

/// The primary content of a datagram.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// A probe: the receiver answers with an [`Ack`](Message::Ack) carrying
    /// the same `seq`, provided it is `target` (an address may have been
    /// taken over by another node since the prober learned it).
    Ping {
        /// Matches the ack to the probe.
        seq: u64,
        /// The node the prober believes it is probing.
        target: NodeId,
    },
    /// Answer to a `Ping`, or relayed answer to a `PingReq`.
    Ack {
        /// The `seq` of the ping (or ping request) being answered.
        seq: u64,
    },
    /// A request to probe `target` on the sender's behalf and forward the
    /// ack, used when a direct probe got no answer in time.
    PingReq {
        /// The requester's probe `seq`, echoed in the forwarded ack.
        seq: u64,
        /// The node to probe.
        target: NodeId,
        /// Where to probe it.
        target_addr: SocketAddr,
    },
    /// Full-state exchange (join, anti-entropy, reconnect). Large states
    /// span several datagrams; every record is self-contained, so chunks
    /// are merged as they arrive and a lost chunk costs only its records.
    Sync {
        /// Whether the receiver should answer with its own state.
        reply: bool,
        /// The sender's view, or part of it.
        members: Vec<MemberRecord>,
    },
    /// No primary content; the packet exists to carry updates.
    Gossip,
}

impl Message {
    /// Short lowercase name, for logs.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Message::Ping { .. } => "ping",
            Message::Ack { .. } => "ack",
            Message::PingReq { .. } => "ping-req",
            Message::Sync { .. } => "sync",
            Message::Gossip => "gossip",
        }
    }

    const fn kind(&self) -> u8 {
        match self {
            Message::Ping { .. } => MSG_PING,
            Message::Ack { .. } => MSG_ACK,
            Message::PingReq { .. } => MSG_PING_REQ,
            Message::Sync { .. } => MSG_SYNC,
            Message::Gossip => MSG_GOSSIP,
        }
    }
}

/// A membership update, spread by gossip.
///
/// Precedence between updates about the same member follows SWIM: a higher
/// incarnation wins; at equal incarnations `Suspect` overrides `Alive`, and
/// `Dead` and `Left` override both. Only the member itself issues `Alive`
/// with a new incarnation, which is how it refutes suspicion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Update {
    /// The member is alive, with this address and metadata.
    Alive {
        /// The member.
        id: NodeId,
        /// Its incarnation.
        incarnation: u64,
        /// Its API address.
        addr: String,
        /// Its gossip address.
        gossip_addr: SocketAddr,
        /// Its metadata.
        meta: Meta,
    },
    /// `from` could not reach the member.
    Suspect {
        /// The member.
        id: NodeId,
        /// The incarnation being suspected.
        incarnation: u64,
        /// The suspecting node (independent suspicions shorten the
        /// timeout).
        from: NodeId,
    },
    /// `from` declared the member failed.
    Dead {
        /// The member.
        id: NodeId,
        /// The incarnation declared dead.
        incarnation: u64,
        /// The declaring node.
        from: NodeId,
    },
    /// The member left gracefully.
    Left {
        /// The member.
        id: NodeId,
        /// Its incarnation when leaving.
        incarnation: u64,
    },
}

impl Update {
    /// The member the update is about.
    #[must_use]
    pub fn node(&self) -> &NodeId {
        match self {
            Update::Alive { id, .. }
            | Update::Suspect { id, .. }
            | Update::Dead { id, .. }
            | Update::Left { id, .. } => id,
        }
    }

    /// The incarnation the update refers to.
    #[must_use]
    pub fn incarnation(&self) -> u64 {
        match self {
            Update::Alive { incarnation, .. }
            | Update::Suspect { incarnation, .. }
            | Update::Dead { incarnation, .. }
            | Update::Left { incarnation, .. } => *incarnation,
        }
    }

    /// Short lowercase name, for logs.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Update::Alive { .. } => "alive",
            Update::Suspect { .. } => "suspect",
            Update::Dead { .. } => "dead",
            Update::Left { .. } => "left",
        }
    }

    /// The `Alive` update describing `member`.
    #[must_use]
    pub fn alive(member: &Member) -> Update {
        Update::Alive {
            id: member.id.clone(),
            incarnation: member.incarnation,
            addr: member.addr.clone(),
            gossip_addr: member.gossip_addr,
            meta: member.meta.clone(),
        }
    }

    /// The encoded form, as it appears inside a packet.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut enc = Encoder::new();
        fields::put_update(&mut enc, self);
        enc.into_vec()
    }
}

/// A member's full state, as exchanged by [`Message::Sync`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberRecord {
    /// The member.
    pub id: NodeId,
    /// Its API address.
    pub addr: String,
    /// Its gossip address.
    pub gossip_addr: SocketAddr,
    /// Its state in the sender's view.
    pub state: MemberState,
    /// Its incarnation in the sender's view.
    pub incarnation: u64,
    /// Its metadata.
    pub meta: Meta,
}
