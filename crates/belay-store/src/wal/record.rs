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
