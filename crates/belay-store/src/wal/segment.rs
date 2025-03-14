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

impl SegmentWriter {
    /// Creates `wal-<first_seq>.log` in `dir` (replacing an empty leftover
    /// of the same name), writes and syncs its header, and syncs the
    /// directory so the segment survives a crash before any record in it
    /// is acknowledged.
    pub(crate) fn create(fs: &dyn Vfs, dir: &Path, first_seq: u64) -> Result<SegmentWriter> {
        let path = dir.join(segment_name(first_seq));
        let mut file = fs.create(&path).map_err(io_err("creating", &path))?;
        file.write_all(&segment_header(first_seq))
            .map_err(io_err("writing", &path))?;
        file.sync().map_err(io_err("syncing", &path))?;
        fs.sync_dir(dir).map_err(io_err("syncing directory", dir))?;
        Ok(SegmentWriter {
            path,
            file,
            first_seq,
            len: SEGMENT_HEADER_LEN as u64,
            synced_len: SEGMENT_HEADER_LEN as u64,
            records: 0,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn first_seq(&self) -> u64 {
        self.first_seq
    }

    /// Bytes in the segment, header included.
    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    pub(crate) fn has_records(&self) -> bool {
        self.records > 0
    }

    /// Frames one record into `out`. Records framed together must be
    /// written with one [`write`](Self::write) before anything else is
    /// framed, because each records the synced length at framing time.
    pub(crate) fn frame(&self, out: &mut Vec<u8>, seq: u64, payload: &[u8]) {
        encode_record(out, seq, self.synced_len, payload);
    }

    /// Appends framed records.
    pub(crate) fn write(&mut self, framed: &[u8], records: u64) -> Result<()> {
        self.file
            .write_all(framed)
            .map_err(io_err("appending to", &self.path))?;
        self.len += framed.len() as u64;
        self.records += records;
        Ok(())
    }

    /// Makes everything written durable. Returns whether a sync was
    /// needed.
    pub(crate) fn sync(&mut self) -> Result<bool> {
        if self.synced_len == self.len {
            return Ok(false);
        }
        self.file.sync().map_err(io_err("syncing", &self.path))?;
        self.synced_len = self.len;
        Ok(true)
    }
}

/// Bytes starting at `offset` that do not form a valid record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Damage {
    pub(crate) offset: usize,
    pub(crate) reason: String,
    /// Where the next valid record starts, if there is one; reading
    /// continues there.
    pub(crate) resume: Option<usize>,
    /// Whether a later record proves the damaged bytes had been synced (its
    /// `synced_to` lies beyond `offset`), which rules out a torn write.
    pub(crate) proven: bool,
}

impl Damage {
    /// Length of the damaged region in a buffer of `len` bytes.
    pub(crate) fn len_in(&self, len: usize) -> usize {
        self.resume.unwrap_or(len) - self.offset
    }
}

/// One step of reading a segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Event<'a> {
    /// A valid record.
    Record(Record<'a>),
    /// A damaged region.
    Damage(Damage),
}

/// Iterates over the records of a segment buffer, reporting damaged
/// regions and skipping past them to the next valid record.
pub(crate) struct RecordReader<'a> {
    buf: &'a [u8],
    pos: usize,
    last_seq: u64,
}
