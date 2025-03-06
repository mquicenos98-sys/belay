//! Snapshot files: the full sorted contents of the store at one sequence
//! number, in the format specified in `crate::format`.
//!
//! Writing streams the entries straight from the map through a buffered
//! file with a running checksum, so a snapshot never needs a second copy
//! of the data in memory. Reading validates the trailer, checksum, header
//! and key order before anything is returned.

use std::collections::BTreeMap;
use std::path::Path;

use belay_common::codec::{Decoder, Encoder};
use belay_common::hash::{crc32c, crc32c_mask, crc32c_unmask, Crc32c};

use crate::batch::MAX_KEY_LEN;
use crate::error::{io_err, Result, StoreError};
use crate::files::{snapshot_name, temp_snapshot_name};
use crate::vfs::{BufferedFile, Vfs};

const MAGIC: &[u8; 8] = b"BELAYSNP";

const END_MAGIC: &[u8; 8] = b"BELAYEND";

const VERSION: u16 = 1;

/// Size of the snapshot header.
pub(crate) const SNAPSHOT_HEADER_LEN: usize = 28;

/// Size of the snapshot trailer.
pub(crate) const SNAPSHOT_TRAILER_LEN: usize = 20;

/// A decoded snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub(crate) seq: u64,
    pub(crate) map: BTreeMap<Vec<u8>, Vec<u8>>,
}

fn header(seq: u64, count: u64) -> [u8; SNAPSHOT_HEADER_LEN] {
    let mut h = [0u8; SNAPSHOT_HEADER_LEN];
    h[0..8].copy_from_slice(MAGIC);
    h[8..10].copy_from_slice(&VERSION.to_le_bytes());
    h[12..20].copy_from_slice(&seq.to_le_bytes());
    h[20..28].copy_from_slice(&count.to_le_bytes());
    h
}

/// Streams the body (header and entries) of a snapshot to `sink`.
fn write_body(
    seq: u64,
    map: &BTreeMap<Vec<u8>, Vec<u8>>,
    mut sink: impl FnMut(&[u8]) -> std::io::Result<()>,
) -> std::io::Result<()> {
    sink(&header(seq, map.len() as u64))?;
    let mut prefix = Encoder::with_capacity(10);
    for (key, value) in map {
        // Lengths go through a small encoder; keys and values are written
        // straight from the map rather than copied into a buffer.
        prefix.clear();
        prefix.put_count(key.len());
        sink(prefix.as_slice())?;
        sink(key)?;
        prefix.clear();
        prefix.put_count(value.len());
        sink(prefix.as_slice())?;
        sink(value)?;
    }
    Ok(())
}

fn trailer(body_len: u64, crc: u32) -> [u8; SNAPSHOT_TRAILER_LEN] {
    let mut t = [0u8; SNAPSHOT_TRAILER_LEN];
    t[0..8].copy_from_slice(&body_len.to_le_bytes());
    t[8..12].copy_from_slice(&crc32c_mask(crc).to_le_bytes());
    t[12..20].copy_from_slice(END_MAGIC);
    t
}

/// Encodes a snapshot in memory; the file writer produces the same bytes.
#[cfg(test)]
pub(crate) fn encode(seq: u64, map: &BTreeMap<Vec<u8>, Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::new();
    write_body(seq, map, |b| {
        out.extend_from_slice(b);
        Ok(())
    })
    .expect("writing to a Vec cannot fail");
    let t = trailer(out.len() as u64, crc32c(&out));
    out.extend_from_slice(&t);
    out
}
