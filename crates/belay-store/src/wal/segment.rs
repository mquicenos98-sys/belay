//! Segment headers, the segment writer, and a reader that classifies
//! damage, following the rules in `crate::format`.

use std::io::Write;
use std::path::{Path, PathBuf};

use belay_common::hash::{crc32c, crc32c_mask, crc32c_unmask};

use super::record::{encode_record, find_next_record, parse_record, Record};
use crate::error::{io_err, Result};
use crate::files::segment_name;
use crate::vfs::{Vfs, VfsFile};

/// Size of the segment header.
pub(crate) const SEGMENT_HEADER_LEN: usize = 24;

const MAGIC: &[u8; 8] = b"BELAYWAL";

const VERSION: u16 = 1;

/// The header of a segment whose first record has sequence number
/// `first_seq`.
pub(crate) fn segment_header(first_seq: u64) -> [u8; SEGMENT_HEADER_LEN] {
    let mut h = [0u8; SEGMENT_HEADER_LEN];
    h[0..8].copy_from_slice(MAGIC);
    h[8..10].copy_from_slice(&VERSION.to_le_bytes());
    // Bytes 10..12 are the reserved flags, zero.
    h[12..20].copy_from_slice(&first_seq.to_le_bytes());
    let crc = crc32c_mask(crc32c(&h[..20]));
    h[20..24].copy_from_slice(&crc.to_le_bytes());
    h
}

/// Validates a segment header, returning its `first_seq`.
pub(crate) fn parse_segment_header(buf: &[u8]) -> Result<u64, String> {
    if buf.len() < SEGMENT_HEADER_LEN {
        return Err(format!(
            "incomplete segment header ({} of {SEGMENT_HEADER_LEN} bytes)",
            buf.len()
        ));
    }
    let h = &buf[..SEGMENT_HEADER_LEN];
    let stored = crc32c_unmask(u32::from_le_bytes(
        h[20..24].try_into().expect("4-byte slice"),
    ));
    if stored != crc32c(&h[..20]) {
        return Err("segment header checksum mismatch".to_string());
    }
    if &h[0..8] != MAGIC {
        return Err("not a belay log segment (bad magic)".to_string());
    }
    let version = u16::from_le_bytes([h[8], h[9]]);
    if version != VERSION {
        return Err(format!("unsupported segment version {version}"));
    }
    Ok(u64::from_le_bytes(
        h[12..20].try_into().expect("8-byte slice"),
    ))
}

/// Appends records to the active segment.
#[derive(Debug)]
pub(crate) struct SegmentWriter {
    path: PathBuf,
    file: Box<dyn VfsFile>,
    first_seq: u64,
    len: u64,
    synced_len: u64,
    records: u64,
}
