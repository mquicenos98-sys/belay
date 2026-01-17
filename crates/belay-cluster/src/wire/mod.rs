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

impl From<&Member> for MemberRecord {
    fn from(m: &Member) -> Self {
        MemberRecord {
            id: m.id.clone(),
            addr: m.addr.clone(),
            gossip_addr: m.gossip_addr,
            state: m.state,
            incarnation: m.incarnation,
            meta: m.meta.clone(),
        }
    }
}

impl Packet {
    /// A packet without piggybacked updates.
    #[must_use]
    pub fn new(sender: NodeId, message: Message) -> Packet {
        Packet {
            sender,
            message,
            updates: Vec::new(),
        }
    }

    /// Encodes the packet as one datagram for the cluster with tag
    /// `cluster` (see [`cluster_tag`]).
    ///
    /// ```
    /// use belay_cluster::wire::{cluster_tag, decode, Message, Packet};
    /// use belay_cluster::NodeId;
    ///
    /// let tag = cluster_tag("belay");
    /// let ping = Packet::new(
    ///     NodeId::new("a").unwrap(),
    ///     Message::Ping { seq: 7, target: NodeId::new("b").unwrap() },
    /// );
    /// let bytes = ping.encode(tag).unwrap();
    /// assert_eq!(decode(&bytes, tag).unwrap(), ping);
    /// assert!(decode(&bytes, cluster_tag("other")).is_err());
    /// ```
    pub fn encode(&self, cluster: u32) -> Result<Vec<u8>, WireError> {
        let frame = FrameWriter::new(cluster, &self.sender, &self.message);
        let updates: Vec<Vec<u8>> = self.updates.iter().map(Update::to_bytes).collect();
        let bytes = frame.finish(updates.iter().map(Vec::as_slice));
        if bytes.len() > MAX_DATAGRAM {
            return Err(WireError::TooLarge(bytes.len()));
        }
        Ok(bytes)
    }
}

/// Decodes and verifies one datagram addressed to the cluster with tag
/// `cluster`.
pub fn decode(bytes: &[u8], cluster: u32) -> Result<Packet, WireError> {
    if bytes.len() < HEADER_LEN {
        return Err(WireError::Truncated(bytes.len()));
    }
    if bytes.len() > MAX_DATAGRAM {
        return Err(WireError::TooLarge(bytes.len()));
    }
    if bytes[0] != PROTOCOL_VERSION {
        return Err(WireError::UnsupportedVersion(bytes[0]));
    }
    let expected = u32::from_le_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
    let actual = crc32c(&bytes[5..]);
    if expected != actual {
        return Err(WireError::ChecksumMismatch { expected, actual });
    }
    let tag = u32::from_le_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]);
    if tag != cluster {
        return Err(WireError::ClusterMismatch {
            expected: cluster,
            actual: tag,
        });
    }
    let mut dec = Decoder::new(bytes);
    dec.get_raw(HEADER_LEN)?;
    let sender = fields::get_node_id(&mut dec)?;
    let message = fields::get_message(&mut dec)?;
    let count = dec.get_uvarint()? as usize;
    let mut updates = Vec::with_capacity(count);
    for _ in 0..count {
        updates.push(fields::get_update(&mut dec)?);
    }
    dec.expect_end()?;
    Ok(Packet {
        sender,
        message,
        updates,
    })
}

/// Builds a datagram whose updates are chosen once the size of the rest is
/// known, which is how gossip gets packed into the space a message leaves.
pub(crate) struct FrameWriter {
    enc: Encoder,
}

impl FrameWriter {
    pub(crate) fn new(cluster: u32, sender: &NodeId, message: &Message) -> FrameWriter {
        let mut enc = Encoder::with_capacity(256);
        enc.put_u8(PROTOCOL_VERSION)
            .put_u32(0)
            .put_u32(cluster)
            .put_str(sender.as_str())
            .put_u8(message.kind());
        fields::put_message_body(&mut enc, message);
        FrameWriter { enc }
    }

    /// Bytes available for encoded updates in a datagram of at most
    /// `limit` bytes.
    pub(crate) fn update_budget(&self, limit: usize) -> usize {
        limit.saturating_sub(self.enc.len() + COUNT_RESERVE)
    }

    /// Appends the pre-encoded `updates` and seals the checksum.
    pub(crate) fn finish<'a, I>(mut self, updates: I) -> Vec<u8>
    where
        I: ExactSizeIterator<Item = &'a [u8]>,
    {
        self.enc.put_count(updates.len());
        for update in updates {
            self.enc.put_raw(update);
        }
        seal(self.enc.into_vec())
    }
}

/// Writes the checksum of everything after it into the header.
fn seal(mut bytes: Vec<u8>) -> Vec<u8> {
    let crc = crc32c(&bytes[5..]);
    bytes[1..5].copy_from_slice(&crc.to_le_bytes());
    bytes
}

/// Encodes `records` as `Sync` datagrams of at most `limit` bytes each,
/// packing as many records per datagram as fit.
pub(crate) fn encode_sync(
    cluster: u32,
    sender: &NodeId,
    reply: bool,
    records: &[MemberRecord],
    limit: usize,
) -> Vec<Vec<u8>> {
    let encoded: Vec<Vec<u8>> = records
        .iter()
        .map(|r| {
            let mut enc = Encoder::new();
            fields::put_record(&mut enc, r);
            enc.into_vec()
        })
        .collect();
    // Header, sender, kind, flags, record count, update count (always 0).
    let base = HEADER_LEN + str_encoded_len(sender.as_str()) + 2 + COUNT_RESERVE + 1;
    let mut datagrams = Vec::new();
    let mut chunk: Vec<&[u8]> = Vec::new();
    let mut size = base;
    for record in &encoded {
        if !chunk.is_empty() && size + record.len() > limit {
            datagrams.push(sync_datagram(cluster, sender, reply, &chunk));
            chunk.clear();
            size = base;
        }
        chunk.push(record);
        size += record.len();
    }
    if !chunk.is_empty() || datagrams.is_empty() {
        datagrams.push(sync_datagram(cluster, sender, reply, &chunk));
    }
    datagrams
}

fn sync_datagram(cluster: u32, sender: &NodeId, reply: bool, records: &[&[u8]]) -> Vec<u8> {
    let mut enc = Encoder::new();
    enc.put_u8(PROTOCOL_VERSION)
        .put_u32(0)
        .put_u32(cluster)
        .put_str(sender.as_str())
        .put_u8(MSG_SYNC)
        .put_u8(if reply { SYNC_REPLY } else { 0 })
        .put_count(records.len());
    for record in records {
        enc.put_raw(record);
    }
    enc.put_count(0);
    seal(enc.into_vec())
}

/// Why a datagram was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WireError {
    /// Shorter than the fixed header; holds the length.
    Truncated(usize),
    /// Larger than [`MAX_DATAGRAM`]; holds the length.
    TooLarge(usize),
    /// Written by an incompatible protocol version.
    UnsupportedVersion(u8),
    /// The content does not match the header checksum.
    ChecksumMismatch {
        /// Checksum from the header.
        expected: u32,
        /// Checksum of the content.
        actual: u32,
    },
    /// The packet belongs to a cluster with another name.
    ClusterMismatch {
        /// This node's cluster tag.
        expected: u32,
        /// The packet's cluster tag.
        actual: u32,
    },
    /// The body is malformed.
    Decode(DecodeError),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::Truncated(len) => write!(
                f,
                "datagram of {len} bytes is shorter than the {HEADER_LEN}-byte header"
            ),
            WireError::TooLarge(len) => write!(
                f,
                "datagram of {len} bytes exceeds the {MAX_DATAGRAM}-byte limit"
            ),
            WireError::UnsupportedVersion(v) => write!(
                f,
                "unsupported protocol version {v} (this node speaks version {PROTOCOL_VERSION})"
            ),
            WireError::ChecksumMismatch { expected, actual } => write!(
                f,
                "checksum mismatch (header {expected:#010x}, content {actual:#010x})"
            ),
            WireError::ClusterMismatch { expected, actual } => write!(
                f,
                "packet is from another cluster (tag {actual:#010x}, expected {expected:#010x})"
            ),
            WireError::Decode(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for WireError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WireError::Decode(e) => Some(e),
            _ => None,
        }
    }
}

impl From<DecodeError> for WireError {
    fn from(e: DecodeError) -> Self {
        WireError::Decode(e)
    }
}

#[cfg(test)]
mod tests;
