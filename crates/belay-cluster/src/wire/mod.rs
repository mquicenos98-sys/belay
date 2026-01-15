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
