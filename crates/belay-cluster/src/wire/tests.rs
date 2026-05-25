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

#[test]
fn header_layout_matches_the_documentation() {
    let bytes = Packet::new(id("a"), Message::Gossip).encode(TAG).unwrap();
    assert_eq!(bytes[0], PROTOCOL_VERSION);
    assert_eq!(
        u32::from_le_bytes(bytes[1..5].try_into().unwrap()),
        crc32c(&bytes[5..])
    );
    assert_eq!(u32::from_le_bytes(bytes[5..9].try_into().unwrap()), TAG);
    // sender "a", kind 5, zero updates
    assert_eq!(&bytes[9..], &[1, b'a', MSG_GOSSIP, 0]);
}

#[test]
fn any_single_bit_flip_is_detected() {
    let packet = Packet {
        sender: id("node-a"),
        message: Message::Ping {
            seq: 77,
            target: id("node-b"),
        },
        updates: vec![Update::Left {
            id: id("node-c"),
            incarnation: 2,
        }],
    };
    let bytes = packet.encode(TAG).unwrap();
    for i in 0..bytes.len() {
        for bit in 0..8 {
            let mut corrupt = bytes.clone();
            corrupt[i] ^= 1 << bit;
            assert!(decode(&corrupt, TAG).is_err(), "flip at byte {i} bit {bit}");
        }
    }
}

#[test]
fn rejects_wrong_version_cluster_and_size() {
    let bytes = Packet::new(id("a"), Message::Ack { seq: 1 })
        .encode(TAG)
        .unwrap();

    let mut v2 = bytes.clone();
    v2[0] = 2;
    assert_eq!(decode(&v2, TAG), Err(WireError::UnsupportedVersion(2)));

    match decode(&bytes, TAG ^ 1) {
        Err(WireError::ClusterMismatch { expected, actual }) => {
            assert_eq!((expected, actual), (TAG ^ 1, TAG));
        }
        other => panic!("expected cluster mismatch, got {other:?}"),
    }

    assert_eq!(decode(&bytes[..8], TAG), Err(WireError::Truncated(8)));
    assert_eq!(
        decode(&vec![1u8; MAX_DATAGRAM + 1], TAG),
        Err(WireError::TooLarge(MAX_DATAGRAM + 1))
    );
    assert_eq!(
        WireError::Truncated(3).to_string(),
        "datagram of 3 bytes is shorter than the 9-byte header"
    );
}

/// Re-seals `body` (everything after the header) so that only the body
/// checks can reject it.
fn forge(body: &[u8]) -> Vec<u8> {
    let mut bytes = vec![PROTOCOL_VERSION, 0, 0, 0, 0];
    bytes.extend_from_slice(&TAG.to_le_bytes());
    bytes.extend_from_slice(body);
    seal(bytes)
}

fn decode_kind(body: &[u8]) -> DecodeErrorKind {
    match decode(&forge(body), TAG) {
        Err(WireError::Decode(e)) => e.kind().clone(),
        other => panic!("expected a decode error, got {other:?}"),
    }
}

#[test]
fn rejects_malformed_bodies_even_with_a_valid_checksum() {
    // Unknown message kind.
    assert!(
        matches!(decode_kind(&[1, b'a', 9, 0]), DecodeErrorKind::Invalid(m) if m.contains("message kind 9"))
    );
    // Invalid sender id.
    assert!(
        matches!(decode_kind(&[1, b'-', MSG_GOSSIP, 0]), DecodeErrorKind::Invalid(m) if m.contains("start with a letter"))
    );
    // Trailing bytes after the updates.
    assert!(matches!(
        decode_kind(&[1, b'a', MSG_GOSSIP, 0, 0]),
        DecodeErrorKind::TrailingBytes(1)
    ));
    // Update count larger than the input.
    assert!(matches!(
        decode_kind(&[1, b'a', MSG_GOSSIP, 100]),
        DecodeErrorKind::CountTooLarge { .. }
    ));
    // Unknown update tag.
    assert!(
        matches!(decode_kind(&[1, b'a', MSG_GOSSIP, 1, 7, 1, b'b', 0]), DecodeErrorKind::Invalid(m) if m.contains("update tag 7"))
    );
    // Unknown sync flag bits.
    assert!(
        matches!(decode_kind(&[1, b'a', MSG_SYNC, 0b10, 0, 0]), DecodeErrorKind::Invalid(m) if m.contains("sync flags"))
    );
    // Unknown address family in a ping request.
    assert!(
        matches!(decode_kind(&[1, b'a', MSG_PING_REQ, 1, 1, b'b', 5, 0, 0]), DecodeErrorKind::Invalid(m) if m.contains("address family 5"))
    );
    // Truncated ping.
    assert!(matches!(
        decode_kind(&[1, b'a', MSG_PING, 1]),
        DecodeErrorKind::UnexpectedEof { .. }
    ));
}

#[test]
fn rejects_invalid_member_records() {
    let mut enc = Encoder::new();
    enc.put_str("a").put_u8(MSG_SYNC).put_u8(0).put_count(1);
    fields::put_record(&mut enc, &record("b", MemberState::Alive));
    enc.put_count(0);
    let mut body = enc.into_vec();
    // The state byte sits right before the incarnation varint (42) and the
    // metadata; patch it to an unknown code.
    let meta_len = 1 + 1 + 4 + 1 + 1;
    let state_at = body.len() - 1 - meta_len - 1 - 1;
    assert_eq!(body[state_at], MemberState::Alive.code());
    body[state_at] = 9;
    assert!(
        matches!(decode_kind(&body), DecodeErrorKind::Invalid(m) if m.contains("member state 9"))
    );
}

#[test]
fn rejects_duplicate_and_invalid_metadata() {
    let mut enc = Encoder::new();
    enc.put_str("a").put_u8(MSG_GOSSIP).put_count(1);
    enc.put_u8(1).put_str("b").put_uvarint(1).put_str("");
    fields::put_socket_addr(&mut enc, addr("10.0.0.1:1"));
    enc.put_count(2)
        .put_str("k")
        .put_str("v1")
        .put_str("k")
        .put_str("v2");
    let body = enc.into_vec();
    assert!(
        matches!(decode_kind(&body), DecodeErrorKind::Invalid(m) if m.contains("duplicate metadata key"))
    );

    let mut enc = Encoder::new();
    enc.put_str("a").put_u8(MSG_GOSSIP).put_count(1);
    enc.put_u8(1).put_str("b").put_uvarint(1).put_str("");
    fields::put_socket_addr(&mut enc, addr("10.0.0.1:1"));
    enc.put_count(1).put_str("weight").put_str("-3");
    assert!(
        matches!(decode_kind(&enc.into_vec()), DecodeErrorKind::Invalid(m) if m.contains("weight"))
    );
}

#[test]
fn random_input_never_panics() {
    let mut rng = Rng::seed_from_u64(7);
    let valid = Packet {
        sender: id("node-a"),
        message: Message::Sync {
            reply: true,
            members: vec![
                record("b", MemberState::Alive),
                record("c", MemberState::Dead),
            ],
        },
        updates: vec![Update::Suspect {
            id: id("c"),
            incarnation: 1,
            from: id("b"),
        }],
    }
    .encode(TAG)
    .unwrap();
    for _ in 0..3_000 {
        // Mutate the body of a valid packet and reseal it, so the fuzzing
        // reaches the field decoders rather than stopping at the checksum.
        let mut body = valid[HEADER_LEN..].to_vec();
        for _ in 0..rng.gen_range(1..4usize) {
            let i = rng.gen_range(0..body.len());
            body[i] = rng.next_u32() as u8;
        }
        let cut = rng.gen_range(0..=body.len());
        let _ = decode(&forge(&body[..cut]), TAG);

        let mut noise = vec![0u8; rng.gen_range(0..64usize)];
        rng.fill_bytes(&mut noise);
        let _ = decode(&noise, TAG);
    }
}

#[test]
fn the_largest_update_fits_a_gossip_packet_of_minimum_size() {
    let update = largest_alive();
    let sender = id(&"s".repeat(MAX_NODE_ID_LEN));
    let frame = FrameWriter::new(TAG, &sender, &Message::Gossip);
    let bytes = update.to_bytes();
    assert!(
        bytes.len() <= frame.update_budget(MIN_PACKET_SIZE),
        "update of {} bytes does not fit budget {}",
        bytes.len(),
        frame.update_budget(MIN_PACKET_SIZE)
    );
    let packet = frame.finish(std::iter::once(bytes.as_slice()));
    assert!(packet.len() <= MIN_PACKET_SIZE);
    let decoded = decode(&packet, TAG).unwrap();
    assert_eq!(decoded.updates, vec![update]);
    // Sanity check on the construction: the metadata is close to its limit.
    if let Update::Alive { meta, .. } = &decoded.updates[0] {
        let used: usize = meta
            .iter()
            .map(|(k, v)| str_encoded_len(k) + str_encoded_len(v))
            .sum();
        assert!(used > MAX_META_BYTES - 40, "{used}");
    }
}

#[test]
fn sync_is_split_into_datagrams_within_the_limit() {
    let sender = id("seed");
    let records: Vec<MemberRecord> = (0..60)
        .map(|i| record(&format!("node-{i:03}"), MemberState::Alive))
        .collect();
    let datagrams = encode_sync(TAG, &sender, true, &records, 1_400);
    assert!(datagrams.len() > 1);
    let mut seen = Vec::new();
    for d in &datagrams {
        assert!(d.len() <= 1_400, "{}", d.len());
        let packet = decode(d, TAG).unwrap();
        assert_eq!(packet.sender, sender);
        match packet.message {
            Message::Sync { reply, members } => {
                assert!(reply);
                seen.extend(members);
            }
            other => panic!("expected sync, got {other:?}"),
        }
    }
    assert_eq!(seen, records);

    // An empty state still produces one (empty) datagram.
    let empty = encode_sync(TAG, &sender, false, &[], 1_400);
    assert_eq!(empty.len(), 1);
}

#[test]
fn a_maximal_record_travels_alone_at_minimum_packet_size() {
    let Update::Alive {
        id,
        incarnation,
        addr,
        gossip_addr,
        meta,
    } = largest_alive()
    else {
        unreachable!("largest_alive builds an Alive update")
    };
    let record = MemberRecord {
        id,
        addr,
        gossip_addr,
        state: MemberState::Suspect,
        incarnation,
        meta,
    };
    let sender = NodeId::new("s".repeat(MAX_NODE_ID_LEN)).unwrap();
    let records = vec![record.clone(), record];
    let datagrams = encode_sync(TAG, &sender, false, &records, MIN_PACKET_SIZE);
    assert_eq!(datagrams.len(), 2);
    for d in datagrams {
        assert!(d.len() <= MIN_PACKET_SIZE, "{}", d.len());
        decode(&d, TAG).unwrap();
    }
}

#[test]
fn cluster_tags_differ_by_name() {
    assert_eq!(cluster_tag("belay"), cluster_tag("belay"));
    assert_ne!(cluster_tag("belay"), cluster_tag("belay-staging"));
    assert_ne!(cluster_tag(""), cluster_tag("a"));
}
