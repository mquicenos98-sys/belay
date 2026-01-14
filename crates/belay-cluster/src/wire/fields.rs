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

const FAMILY_V4: u8 = 4;

const FAMILY_V6: u8 = 6;

pub(super) fn put_message_body(enc: &mut Encoder, message: &Message) {
    match message {
        Message::Ping { seq, target } => {
            enc.put_uvarint(*seq).put_str(target.as_str());
        }
        Message::Ack { seq } => {
            enc.put_uvarint(*seq);
        }
        Message::PingReq {
            seq,
            target,
            target_addr,
        } => {
            enc.put_uvarint(*seq).put_str(target.as_str());
            put_socket_addr(enc, *target_addr);
        }
        Message::Sync { reply, members } => {
            enc.put_u8(if *reply { SYNC_REPLY } else { 0 })
                .put_count(members.len());
            for record in members {
                put_record(enc, record);
            }
        }
        Message::Gossip => {}
    }
}

pub(super) fn get_message(dec: &mut Decoder<'_>) -> Result<Message, DecodeError> {
    let kind_at = dec.clone();
    Ok(match dec.get_u8()? {
        MSG_PING => Message::Ping {
            seq: dec.get_uvarint()?,
            target: get_node_id(dec)?,
        },
        MSG_ACK => Message::Ack {
            seq: dec.get_uvarint()?,
        },
        MSG_PING_REQ => Message::PingReq {
            seq: dec.get_uvarint()?,
            target: get_node_id(dec)?,
            target_addr: get_socket_addr(dec)?,
        },
        MSG_SYNC => {
            let flags_at = dec.clone();
            let flags = dec.get_u8()?;
            if flags & !SYNC_REPLY != 0 {
                return Err(flags_at.invalid(format!("unknown sync flags {flags:#04x}")));
            }
            let count = dec.get_count()?;
            let mut members = Vec::with_capacity(count);
            for _ in 0..count {
                members.push(get_record(dec)?);
            }
            Message::Sync {
                reply: flags & SYNC_REPLY != 0,
                members,
            }
        }
        MSG_GOSSIP => Message::Gossip,
        other => return Err(kind_at.invalid(format!("unknown message kind {other}"))),
    })
}

pub(super) fn put_update(enc: &mut Encoder, update: &Update) {
    match update {
        Update::Alive {
            id,
            incarnation,
            addr,
            gossip_addr,
            meta,
        } => {
            enc.put_u8(UPD_ALIVE)
                .put_str(id.as_str())
                .put_uvarint(*incarnation)
                .put_str(addr);
            put_socket_addr(enc, *gossip_addr);
            put_meta(enc, meta);
        }
        Update::Suspect {
            id,
            incarnation,
            from,
        } => {
            enc.put_u8(UPD_SUSPECT)
                .put_str(id.as_str())
                .put_uvarint(*incarnation)
                .put_str(from.as_str());
        }
        Update::Dead {
            id,
            incarnation,
            from,
        } => {
            enc.put_u8(UPD_DEAD)
                .put_str(id.as_str())
                .put_uvarint(*incarnation)
                .put_str(from.as_str());
        }
        Update::Left { id, incarnation } => {
            enc.put_u8(UPD_LEFT)
                .put_str(id.as_str())
                .put_uvarint(*incarnation);
        }
    }
}

pub(super) fn get_update(dec: &mut Decoder<'_>) -> Result<Update, DecodeError> {
    let tag_at = dec.clone();
    let tag = dec.get_u8()?;
    let id = get_node_id(dec)?;
    let incarnation = dec.get_uvarint()?;
    Ok(match tag {
        UPD_ALIVE => Update::Alive {
            id,
            incarnation,
            addr: get_addr(dec)?,
            gossip_addr: get_socket_addr(dec)?,
            meta: get_meta(dec)?,
        },
        UPD_SUSPECT => Update::Suspect {
            id,
            incarnation,
            from: get_node_id(dec)?,
        },
        UPD_DEAD => Update::Dead {
            id,
            incarnation,
            from: get_node_id(dec)?,
        },
        UPD_LEFT => Update::Left { id, incarnation },
        other => return Err(tag_at.invalid(format!("unknown update tag {other}"))),
    })
}

pub(super) fn put_record(enc: &mut Encoder, record: &MemberRecord) {
    enc.put_str(record.id.as_str()).put_str(&record.addr);
    put_socket_addr(enc, record.gossip_addr);
    enc.put_u8(record.state.code())
        .put_uvarint(record.incarnation);
    put_meta(enc, &record.meta);
}

fn get_record(dec: &mut Decoder<'_>) -> Result<MemberRecord, DecodeError> {
    let id = get_node_id(dec)?;
    let addr = get_addr(dec)?;
    let gossip_addr = get_socket_addr(dec)?;
    let state_at = dec.clone();
    let code = dec.get_u8()?;
    let state = MemberState::from_code(code)
        .ok_or_else(|| state_at.invalid(format!("unknown member state {code}")))?;
    Ok(MemberRecord {
        id,
        addr,
        gossip_addr,
        state,
        incarnation: dec.get_uvarint()?,
        meta: get_meta(dec)?,
    })
}

pub(super) fn get_node_id(dec: &mut Decoder<'_>) -> Result<NodeId, DecodeError> {
    let at = dec.clone();
    let s = dec.get_str()?;
    NodeId::new(s).map_err(|e| at.invalid(e.to_string()))
}

fn get_addr(dec: &mut Decoder<'_>) -> Result<String, DecodeError> {
    let at = dec.clone();
    let s = dec.get_str()?;
    validate_addr(s).map_err(|e| at.invalid(e.to_string()))?;
    Ok(s.to_owned())
}
