//! Framing of individual log records; the layout is specified in
//! `crate::format`.

use belay_common::hash::{crc32c, crc32c_mask, crc32c_unmask};

use crate::batch::MAX_BATCH_LEN;

/// Bytes before a record's payload.
pub(crate) const RECORD_HEADER_LEN: usize = 25;
/// The only record kind so far: a write batch.
pub(crate) const KIND_BATCH: u8 = 1;

/// Appends one framed record to `out`.
pub(crate) fn encode_record(out: &mut Vec<u8>, seq: u64, synced_to: u64, payload: &[u8]) {
    debug_assert!(payload.len() <= MAX_BATCH_LEN, "batch size is validated");
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&synced_to.to_le_bytes());
    out.push(KIND_BATCH);
    out.extend_from_slice(payload);
    let crc = crc32c_mask(crc32c(&out[start + 4..]));
    out[start..start + 4].copy_from_slice(&crc.to_le_bytes());
}

/// A record that passed validation, borrowing its payload from the
/// segment buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Record<'a> {
    pub(crate) offset: usize,
    pub(crate) seq: u64,
    pub(crate) synced_to: u64,
    pub(crate) kind: u8,
    pub(crate) payload: &'a [u8],
}

impl Record<'_> {
    /// Offset just past this record.
    pub(crate) fn end(&self) -> usize {
        self.offset + RECORD_HEADER_LEN + self.payload.len()
    }
}

fn u32_at(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(buf[at..at + 4].try_into().expect("4-byte slice"))
}

fn u64_at(buf: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(buf[at..at + 8].try_into().expect("8-byte slice"))
}

/// Parses and verifies the record at `offset`, or explains why there is
/// no valid record there.
pub(crate) fn parse_record(buf: &[u8], offset: usize) -> Result<Record<'_>, String> {
    let rest = buf.len().saturating_sub(offset);
    if rest < RECORD_HEADER_LEN {
        return Err(format!(
            "incomplete record header ({rest} of {RECORD_HEADER_LEN} bytes)"
        ));
    }
    let h = &buf[offset..offset + RECORD_HEADER_LEN];
    let len = u32_at(h, 4) as usize;
    if len > MAX_BATCH_LEN {
        return Err(format!("record length {len} exceeds the limit"));
    }
    let end = offset + RECORD_HEADER_LEN + len;
    if end > buf.len() {
        return Err(format!(
            "record of {len} bytes extends {} bytes past the end of the file",
            end - buf.len()
        ));
    }
    let stored = crc32c_unmask(u32_at(h, 0));
    let actual = crc32c(&buf[offset + 4..end]);
    if stored != actual {
        return Err(format!(
            "checksum mismatch (stored {stored:08x}, computed {actual:08x})"
        ));
    }
    // The checksum passed, so the remaining checks catch writer bugs or
    // foreign data rather than damage.
    let kind = h[24];
    if kind != KIND_BATCH {
        return Err(format!("unknown record kind {kind}"));
    }
    let synced_to = u64_at(h, 16);
    if synced_to > offset as u64 {
        return Err(format!(
            "record claims the file was synced to {synced_to}, beyond its own offset"
        ));
    }
    Ok(Record {
        offset,
        seq: u64_at(h, 8),
        synced_to,
        kind,
        payload: &buf[offset + RECORD_HEADER_LEN..end],
    })
}

/// The first valid record starting after `from` whose sequence number
/// exceeds `min_seq`, found by trying every offset.
///
/// Cheap header checks run before the checksum, so random bytes and
/// zero-filled pages are skipped at close to memory speed; a false
/// positive needs a matching 32-bit CRC.
pub(crate) fn find_next_record(buf: &[u8], from: usize, min_seq: u64) -> Option<Record<'_>> {
    let last_start = buf.len().checked_sub(RECORD_HEADER_LEN)?;
    (from..=last_start).find_map(|offset| {
        let h = &buf[offset..offset + RECORD_HEADER_LEN];
        let len = u32_at(h, 4) as usize;
        let plausible = h[24] == KIND_BATCH
            && len <= buf.len() - offset - RECORD_HEADER_LEN
            && u64_at(h, 8) > min_seq
            && u64_at(h, 16) <= offset as u64;
        if plausible {
            parse_record(buf, offset).ok()
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn framed(records: &[(u64, u64, &[u8])]) -> Vec<u8> {
        let mut out = vec![0xEE; 7]; // stand-in for a segment header
        for &(seq, synced, payload) in records {
            encode_record(&mut out, seq, synced, payload);
        }
        out
    }

    #[test]
    fn header_layout_is_as_documented() {
        let buf = framed(&[(0x0102, 7, b"xyz")]);
        let r = &buf[7..];
        assert_eq!(r.len(), RECORD_HEADER_LEN + 3);
        assert_eq!(&r[4..8], &3u32.to_le_bytes());
        assert_eq!(&r[8..16], &0x0102u64.to_le_bytes());
        assert_eq!(&r[16..24], &7u64.to_le_bytes());
        assert_eq!(r[24], KIND_BATCH);
        assert_eq!(&r[25..], b"xyz");
        let crc = crc32c_unmask(u32_at(r, 0));
        assert_eq!(crc, crc32c(&r[4..]));
    }

    #[test]
    fn parses_consecutive_records() {
        let buf = framed(&[(1, 0, b"a"), (2, 7, b""), (3, 7, &[0u8; 300])]);
        let mut offset = 7;
        for seq in 1..=3 {
            let rec = parse_record(&buf, offset).expect("valid");
            assert_eq!(rec.seq, seq);
            offset = rec.end();
        }
        assert_eq!(offset, buf.len());
    }

    #[test]
    fn every_truncation_and_bit_flip_is_detected() {
        let buf = framed(&[(5, 0, b"payload bytes")]);
        for cut in 7..buf.len() {
            assert!(parse_record(&buf[..cut], 7).is_err(), "cut {cut}");
        }
        for i in 7..buf.len() {
            for bit in 0..8 {
                let mut bad = buf.clone();
                bad[i] ^= 1 << bit;
                assert!(parse_record(&bad, 7).is_err(), "byte {i} bit {bit}");
            }
        }
    }

    #[test]
    fn explains_why_a_record_is_invalid() {
        let buf = framed(&[(1, 0, b"abc")]);
        assert!(parse_record(&buf[..10], 7)
            .unwrap_err()
            .starts_with("incomplete record header"));
        assert!(parse_record(&buf[..buf.len() - 1], 7)
            .unwrap_err()
            .contains("past the end of the file"));
        let mut huge = buf.clone();
        huge[11..15].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse_record(&huge, 7)
            .unwrap_err()
            .contains("exceeds the limit"));
        let mut flipped = buf.clone();
        *flipped.last_mut().expect("non-empty") ^= 1;
        assert!(parse_record(&flipped, 7)
            .unwrap_err()
            .starts_with("checksum mismatch"));
        // A record claiming durability past itself is rejected even with a
        // valid checksum.
        let bad_sync = framed(&[(1, 100, b"")]);
        assert!(parse_record(&bad_sync, 7)
            .unwrap_err()
            .contains("beyond its own offset"));
    }

    #[test]
    fn resynchronizes_after_garbage() {
        let mut buf = framed(&[(1, 0, b"first")]);
        let garbage_at = buf.len();
        buf.extend_from_slice(&[0u8; 40]);
        buf.extend_from_slice(&[0xAB; 13]);
        let second_at = buf.len();
        encode_record(&mut buf, 2, 7, b"second");
        let found = find_next_record(&buf, garbage_at, 1).expect("finds the second record");
        assert_eq!(found.offset, second_at);
        assert_eq!(found.payload, b"second");
        // Older sequence numbers are not accepted as a resync point.
        assert!(find_next_record(&buf, garbage_at, 2).is_none());
        assert!(find_next_record(&buf[..10], 0, 0).is_none());
    }
}
