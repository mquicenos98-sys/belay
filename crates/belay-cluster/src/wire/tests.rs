use super::*;
use crate::member::{MAX_ADDR_LEN, MAX_META_BYTES, MAX_NODE_ID_LEN};
use belay_common::codec::DecodeErrorKind;
use belay_common::Rng;

const TAG: u32 = 0x1234_5678;

fn id(s: &str) -> NodeId {
    NodeId::new(s).unwrap()
}

fn addr(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

fn meta(pairs: &[(&str, &str)]) -> Meta {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

fn record(name: &str, state: MemberState) -> MemberRecord {
    MemberRecord {
        id: id(name),
        addr: format!("{name}.internal:7070"),
        gossip_addr: addr("10.1.2.3:7946"),
        state,
        incarnation: 42,
        meta: meta(&[("zone", "b")]),
    }
}

fn round_trip(packet: &Packet) {
    let bytes = packet.encode(TAG).unwrap();
    assert_eq!(&decode(&bytes, TAG).unwrap(), packet);
}

/// The largest update the member limits allow.
fn largest_alive() -> Update {
    let mut big = Meta::new();
    // Fill metadata up to the encoded limit with 30-byte values.
    let mut i = 0;
    loop {
        let key = format!("k{i:02}");
        let mut candidate = big.clone();
        candidate.insert(key, "v".repeat(30));
        if crate::member::validate_meta(&candidate).is_err() {
            break;
        }
        big = candidate;
        i += 1;
    }
    Update::Alive {
        id: id(&"n".repeat(MAX_NODE_ID_LEN)),
        incarnation: u64::MAX,
        addr: "a".repeat(MAX_ADDR_LEN),
        gossip_addr: addr("[2001:db8::1]:65535"),
        meta: big,
    }
}

#[test]
fn every_message_kind_round_trips() {
    let sender = id("node-a");
    let messages = [
        Message::Ping {
            seq: 0,
            target: id("node-b"),
        },
        Message::Ack { seq: u64::MAX },
        Message::PingReq {
            seq: 300,
            target: id("node-c"),
            target_addr: addr("[::1]:7946"),
        },
        Message::Sync {
            reply: true,
            members: vec![
                record("a", MemberState::Alive),
                record("b", MemberState::Suspect),
                record("c", MemberState::Dead),
                record("d", MemberState::Left),
            ],
        },
        Message::Sync {
            reply: false,
            members: Vec::new(),
        },
        Message::Gossip,
    ];
    for message in messages {
        round_trip(&Packet::new(sender.clone(), message));
    }
}

#[test]
fn every_update_kind_round_trips_piggybacked() {
    let packet = Packet {
        sender: id("a"),
        message: Message::Ack { seq: 9 },
        updates: vec![
            Update::Alive {
                id: id("b"),
                incarnation: 3,
                addr: String::new(),
                gossip_addr: addr("192.168.0.9:1"),
                meta: meta(&[("weight", "2"), ("zone", "eu")]),
            },
            Update::Suspect {
                id: id("c"),
                incarnation: 4,
                from: id("a"),
            },
            Update::Dead {
                id: id("d"),
                incarnation: 5,
                from: id("b"),
            },
            Update::Left {
                id: id("e"),
                incarnation: 6,
            },
        ],
    };
    round_trip(&packet);
    assert_eq!(packet.updates[1].node(), "c");
    assert_eq!(packet.updates[2].incarnation(), 5);
    let names: Vec<&str> = packet.updates.iter().map(Update::name).collect();
    assert_eq!(names, ["alive", "suspect", "dead", "left"]);
}
