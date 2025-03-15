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

impl<'a> RecordReader<'a> {
    /// A reader starting at `start` (normally just past the header).
    /// `last_seq` is the sequence number of the record before this
    /// segment; resynchronization only accepts records after it.
    pub(crate) fn new(buf: &'a [u8], start: usize, last_seq: u64) -> RecordReader<'a> {
        RecordReader {
            buf,
            pos: start.min(buf.len()),
            last_seq,
        }
    }

    /// The largest `synced_to` among valid records from `first` onwards,
    /// following the record chain and resynchronizing across damage.
    fn max_synced_after(&self, first: Record<'a>) -> u64 {
        let mut max = first.synced_to;
        let mut last_seq = first.seq;
        let mut pos = first.end();
        while pos < self.buf.len() {
            let next = match parse_record(self.buf, pos) {
                Ok(rec) => Some(rec),
                Err(_) => find_next_record(self.buf, pos + 1, last_seq),
            };
            let Some(rec) = next else { break };
            max = max.max(rec.synced_to);
            last_seq = last_seq.max(rec.seq);
            pos = rec.end();
        }
        max
    }

    /// Classifies damage found at `offset`.
    pub(crate) fn damage_at(&self, offset: usize, reason: String) -> Damage {
        let next = find_next_record(self.buf, offset + 1, self.last_seq);
        let proven = next.is_some_and(|rec| self.max_synced_after(rec) > offset as u64);
        Damage {
            offset,
            reason,
            resume: next.map(|rec| rec.offset),
            proven,
        }
    }
}

impl<'a> Iterator for RecordReader<'a> {
    type Item = Event<'a>;

    fn next(&mut self) -> Option<Event<'a>> {
        if self.pos >= self.buf.len() {
            return None;
        }
        match parse_record(self.buf, self.pos) {
            Ok(rec) => {
                self.pos = rec.end();
                self.last_seq = self.last_seq.max(rec.seq);
                Some(Event::Record(rec))
            }
            Err(reason) => {
                let damage = self.damage_at(self.pos, reason);
                self.pos = damage.resume.unwrap_or(self.buf.len());
                Some(Event::Damage(damage))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::{Crash, MemFs};

    fn records(events: &[Event<'_>]) -> Vec<u64> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Record(r) => Some(r.seq),
                Event::Damage(_) => None,
            })
            .collect()
    }

    /// Builds a segment the way the writer does: groups of records, each
    /// group optionally synced.
    fn build(groups: &[(&[u64], bool)]) -> (MemFs, PathBuf, Vec<usize>) {
        let fs = MemFs::new();
        let dir = Path::new("db");
        fs.create_dir_all(dir).expect("mkdir");
        let first = groups[0].0[0];
        let mut w = SegmentWriter::create(&fs, dir, first).expect("create");
        let mut offsets = Vec::new();
        for (seqs, sync) in groups {
            let mut buf = Vec::new();
            for &seq in *seqs {
                offsets.push(w.len() as usize + buf.len());
                w.frame(&mut buf, seq, format!("batch {seq}").as_bytes());
            }
            w.write(&buf, seqs.len() as u64).expect("write");
            if *sync {
                w.sync().expect("sync");
            }
        }
        (fs, w.path().to_path_buf(), offsets)
    }

    #[test]
    fn header_round_trips_and_rejects_damage() {
        let h = segment_header(77);
        assert_eq!(&h[..8], b"BELAYWAL");
        assert_eq!(parse_segment_header(&h), Ok(77));
        assert!(parse_segment_header(&h[..10])
            .unwrap_err()
            .contains("incomplete"));
        let mut bad = h;
        bad[13] ^= 1;
        assert_eq!(
            parse_segment_header(&bad).unwrap_err(),
            "segment header checksum mismatch"
        );
        let mut other = h;
        other[0] = b'X';
        let crc = crc32c_mask(crc32c(&other[..20]));
        other[20..24].copy_from_slice(&crc.to_le_bytes());
        assert!(parse_segment_header(&other)
            .unwrap_err()
            .contains("bad magic"));
        let mut future = h;
        future[8] = 2;
        let crc = crc32c_mask(crc32c(&future[..20]));
        future[20..24].copy_from_slice(&crc.to_le_bytes());
        assert!(parse_segment_header(&future)
            .unwrap_err()
            .contains("unsupported segment version 2"));
    }

    #[test]
    fn writer_creates_a_durable_segment() {
        let (fs, path, _) = build(&[(&[5, 6], true), (&[7], false)]);
        assert_eq!(path, Path::new("db").join(segment_name(5)));
        let after = fs.crash_copy(&Crash::PowerLoss);
        let buf = after.read(&path).expect("segment survives");
        assert_eq!(parse_segment_header(&buf), Ok(5));
        let events: Vec<Event<'_>> = RecordReader::new(&buf, SEGMENT_HEADER_LEN, 4).collect();
        assert_eq!(records(&events), vec![5, 6]);
    }

    #[test]
    fn records_carry_the_synced_length_at_framing_time() {
        let (fs, path, offsets) = build(&[(&[1, 2], true), (&[3], false), (&[4], true)]);
        let buf = fs.read(&path).expect("read");
        let synced: Vec<u64> = RecordReader::new(&buf, SEGMENT_HEADER_LEN, 0)
            .map(|e| match e {
                Event::Record(r) => r.synced_to,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        let g2 = offsets[2] as u64;
        let header = SEGMENT_HEADER_LEN as u64;
        // Group 1 saw only the header synced; groups 2 and 3 saw group 1.
        assert_eq!(synced, vec![header, header, g2, g2]);
    }

    #[test]
    fn damage_in_an_unsynced_group_is_not_proven() {
        // Group 2 = records 3, 4 unsynced; damage record 3 while record 4
        // is intact, as out-of-order page writeback can leave it.
        let (fs, path, offsets) = build(&[(&[1, 2], true), (&[3, 4], false)]);
        let mut buf = fs.read(&path).expect("read");
        buf[offsets[2] + 30] ^= 0xFF;
        let events: Vec<Event<'_>> = RecordReader::new(&buf, SEGMENT_HEADER_LEN, 0).collect();
        match &events[2] {
            Event::Damage(d) => {
                assert_eq!(d.offset, offsets[2]);
                assert_eq!(d.resume, Some(offsets[3]));
                assert_eq!(d.len_in(buf.len()), offsets[3] - offsets[2]);
                assert!(!d.proven, "record 4 was written before record 3 was synced");
            }
            other => panic!("expected damage, got {other:?}"),
        }
        assert_eq!(records(&events), vec![1, 2, 4]);
    }

    #[test]
    fn damage_before_a_later_sync_is_proven() {
        let (fs, path, offsets) = build(&[(&[1], true), (&[2], true), (&[3], false)]);
        let mut buf = fs.read(&path).expect("read");
        buf[offsets[1] + 26] ^= 0x01;
        let mut reader = RecordReader::new(&buf, SEGMENT_HEADER_LEN, 0);
        assert!(matches!(reader.next(), Some(Event::Record(r)) if r.seq == 1));
        match reader.next() {
            Some(Event::Damage(d)) => {
                assert!(d.proven, "record 3 says record 2 had been synced");
                assert_eq!(d.resume, Some(offsets[2]));
            }
            other => panic!("expected damage, got {other:?}"),
        }
        assert!(matches!(reader.next(), Some(Event::Record(r)) if r.seq == 3));
        assert!(reader.next().is_none());
    }

    #[test]
    fn truncated_and_zero_filled_tails_are_unproven_damage_without_resume() {
        let (fs, path, offsets) = build(&[(&[1, 2], true), (&[3], false)]);
        let full = fs.read(&path).expect("read");
        for cut in offsets[2] + 1..full.len() {
            let events: Vec<Event<'_>> =
                RecordReader::new(&full[..cut], SEGMENT_HEADER_LEN, 0).collect();
            assert_eq!(records(&events), vec![1, 2], "cut {cut}");
            assert!(matches!(
                events.last(),
                Some(Event::Damage(Damage {
                    resume: None,
                    proven: false,
                    ..
                }))
            ));
        }
        let mut zeroed = full.clone();
        zeroed.extend_from_slice(&[0u8; 4096]);
        let events: Vec<Event<'_>> = RecordReader::new(&zeroed, SEGMENT_HEADER_LEN, 0).collect();
        assert_eq!(records(&events), vec![1, 2, 3]);
        assert_eq!(events.len(), 4);
    }
}
