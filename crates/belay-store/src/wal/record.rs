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
