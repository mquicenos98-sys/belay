//! Field-level encoding shared by messages, updates and member records.
//!
//! Every getter validates what it reads with the same rules the local node
//! applies to its own configuration, so a peer can never introduce an id,
//! address or metadata map that this node could not have produced itself.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use belay_common::codec::{DecodeError, Decoder, Encoder};

use super::{
    MemberRecord, Message, Update, MSG_ACK, MSG_GOSSIP, MSG_PING, MSG_PING_REQ, MSG_SYNC,
    SYNC_REPLY,
};
use crate::member::{validate_addr, validate_meta, MemberState, Meta, NodeId};

const UPD_ALIVE: u8 = 1;

const UPD_SUSPECT: u8 = 2;

const UPD_DEAD: u8 = 3;

const UPD_LEFT: u8 = 4;
