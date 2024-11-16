//! A miniature append-only record file built from `codec`, `hash`, `fs` and
//! `id`, the way the storage engine frames its write-ahead log: each record
//! is `[len: u32][masked crc32c: u32][payload]`.

use belay_common::codec::{DecodeErrorKind, Decoder, Encoder};
use belay_common::fs::{atomic_write, TempDir};
use belay_common::hash::{crc32c, crc32c_mask, crc32c_unmask};
use belay_common::id::UlidGenerator;
use belay_common::rand::Rng;
use belay_common::{Timestamp, Ulid};

#[derive(Debug, PartialEq)]
struct Entry {
    id: Ulid,
    at: Timestamp,
    queue: String,
    attempt: i64,
}

fn encode_entry(e: &Entry) -> Vec<u8> {
    let mut enc = Encoder::new();
    enc.put_raw(&e.id.to_bytes())
        .put_ivarint(e.at.as_millis())
        .put_str(&e.queue)
        .put_ivarint(e.attempt);
    enc.into_vec()
}

fn decode_entry(bytes: &[u8]) -> Result<Entry, belay_common::codec::DecodeError> {
    let mut dec = Decoder::new(bytes);
    let entry = Entry {
        id: Ulid::from_bytes(dec.get_array()?),
        at: Timestamp::from_millis(dec.get_ivarint()?),
        queue: dec.get_str()?.to_owned(),
        attempt: dec.get_ivarint()?,
    };
    dec.expect_end()?;
    Ok(entry)
}

fn frame(records: &[Vec<u8>]) -> Vec<u8> {
    let mut enc = Encoder::new();
    for payload in records {
        enc.put_u32(payload.len() as u32)
            .put_u32(crc32c_mask(crc32c(payload)))
            .put_raw(payload);
    }
    enc.into_vec()
}

/// Reads frames until the end or the first damaged one; returns the good
/// payloads and, if reading stopped early, the offset of the damage.
fn unframe(bytes: &[u8]) -> (Vec<Vec<u8>>, Option<usize>) {
    let mut dec = Decoder::new(bytes);
    let mut out = Vec::new();
    while !dec.is_empty() {
        let start = dec.position();
        let read = (|| {
            let len = dec.get_u32()? as usize;
            let crc = crc32c_unmask(dec.get_u32()?);
            let payload = dec.get_raw(len)?;
            if crc32c(payload) != crc {
                return Err(dec.invalid("checksum mismatch"));
            }
            Ok(payload.to_vec())
        })();
        match read {
            Ok(payload) => out.push(payload),
            Err(_) => return (out, Some(start)),
        }
    }
    (out, None)
}

fn sample_entries(n: usize) -> Vec<Entry> {
    let mut ids = UlidGenerator::with_rng(Rng::seed_from_u64(3));
    let mut rng = Rng::seed_from_u64(4);
    let base = Timestamp::parse_rfc3339("2025-06-01T12:00:00Z").expect("valid timestamp");
    (0..n)
        .map(|i| {
            let at = base + std::time::Duration::from_millis(i as u64 * 7);
            Entry {
                id: ids.generate(at).expect("id space"),
                at,
                queue: format!("queue-{}", rng.gen_range(0..5u32)),
                attempt: rng.gen_range(-1..30i64),
            }
        })
        .collect()
}

#[test]
fn records_survive_a_round_trip_through_a_file() {
    let dir = TempDir::with_prefix("belay-it-log").expect("temp dir");
    let path = dir.join("wal-00000000000000000001.log");
    let entries = sample_entries(200);
    let payloads: Vec<Vec<u8>> = entries.iter().map(encode_entry).collect();
    atomic_write(&path, &frame(&payloads)).expect("write log");

    let bytes = std::fs::read(&path).expect("read log");
    let (read, damage) = unframe(&bytes);
    assert_eq!(damage, None);
    let decoded: Vec<Entry> = read
        .iter()
        .map(|p| decode_entry(p).expect("valid entry"))
        .collect();
    assert_eq!(decoded, entries);
    assert!(decoded.windows(2).all(|w| w[0].id < w[1].id));
}

#[test]
fn every_truncation_point_yields_a_clean_prefix() {
    let entries = sample_entries(5);
    let payloads: Vec<Vec<u8>> = entries.iter().map(encode_entry).collect();
    let bytes = frame(&payloads);
    let boundaries: Vec<usize> = payloads
        .iter()
        .scan(0, |end, p| {
            *end += 8 + p.len();
            Some(*end)
        })
        .collect();
    for cut in 0..bytes.len() {
        let (read, damage) = unframe(&bytes[..cut]);
        let complete = boundaries.iter().filter(|&&b| b <= cut).count();
        assert_eq!(read, payloads[..complete], "cut at {cut}");
        let torn = !boundaries.contains(&cut) && cut != 0;
        assert_eq!(damage.is_some(), torn, "cut at {cut}");
    }
}

#[test]
fn flipped_bits_are_caught_by_the_checksum() {
    let payloads: Vec<Vec<u8>> = sample_entries(3).iter().map(encode_entry).collect();
    let clean = frame(&payloads);
    let first_len = 8 + payloads[0].len();
    // Flip one bit in each payload byte of the second record.
    for offset in first_len + 8..first_len + 8 + payloads[1].len() {
        let mut bytes = clean.clone();
        bytes[offset] ^= 0x10;
        let (read, damage) = unframe(&bytes);
        assert_eq!(read.len(), 1, "flip at {offset}");
        assert_eq!(damage, Some(first_len));
    }
}

#[test]
fn damaged_payloads_report_where_decoding_failed() {
    let entry = &sample_entries(1)[0];
    let mut payload = encode_entry(entry);
    payload.push(0xFF);
    let err = decode_entry(&payload).unwrap_err();
    assert_eq!(err.kind(), &DecodeErrorKind::TrailingBytes(1));
    assert_eq!(err.offset(), payload.len() - 1);
    let err = decode_entry(&payload[..10]).unwrap_err();
    assert!(matches!(err.kind(), DecodeErrorKind::UnexpectedEof { .. }));
}
