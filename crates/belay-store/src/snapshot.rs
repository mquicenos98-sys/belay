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

/// Writes `snap-<seq>.db` atomically and returns its size in bytes.
pub(crate) fn write(
    fs: &dyn Vfs,
    dir: &Path,
    seq: u64,
    map: &BTreeMap<Vec<u8>, Vec<u8>>,
) -> Result<u64> {
    let tmp = dir.join(temp_snapshot_name(seq));
    let path = dir.join(snapshot_name(seq));
    let result = (|| {
        let file = fs.create(&tmp).map_err(io_err("creating", &tmp))?;
        let mut out = BufferedFile::new(file);
        let mut crc = Crc32c::new();
        write_body(seq, map, |bytes| {
            crc.update(bytes);
            out.write(bytes)
        })
        .map_err(io_err("writing", &tmp))?;
        let body_len = out.written();
        out.write(&trailer(body_len, crc.finalize()))
            .map_err(io_err("writing", &tmp))?;
        let size = out.written();
        out.finish().map_err(io_err("syncing", &tmp))?;
        fs.rename(&tmp, &path).map_err(io_err("renaming", &tmp))?;
        fs.sync_dir(dir).map_err(io_err("syncing directory", dir))?;
        Ok(size)
    })();
    if result.is_err() {
        // Best effort: recovery deletes leftover temporary files anyway.
        let _ = fs.remove(&tmp);
    }
    result
}

/// Reads and fully validates the snapshot at `path`.
pub(crate) fn read(fs: &dyn Vfs, path: &Path) -> Result<Snapshot> {
    let buf = fs.read(path).map_err(io_err("reading", path))?;
    decode(&buf).map_err(|(offset, reason)| StoreError::corrupt(path, offset as u64, reason))
}

/// The sequence number and entry count from a snapshot header, without
/// validating the rest of the file.
pub(crate) fn peek_header(buf: &[u8]) -> Option<(u64, u64)> {
    if buf.len() < SNAPSHOT_HEADER_LEN || &buf[0..8] != MAGIC {
        return None;
    }
    let seq = u64::from_le_bytes(buf[12..20].try_into().ok()?);
    let count = u64::from_le_bytes(buf[20..28].try_into().ok()?);
    Some((seq, count))
}

/// Decodes a snapshot, reporting the offset and nature of any damage.
pub(crate) fn decode(buf: &[u8]) -> Result<Snapshot, (usize, String)> {
    let min = SNAPSHOT_HEADER_LEN + SNAPSHOT_TRAILER_LEN;
    if buf.len() < min {
        return Err((
            0,
            format!(
                "file is {} bytes, shorter than an empty snapshot",
                buf.len()
            ),
        ));
    }
    let t_at = buf.len() - SNAPSHOT_TRAILER_LEN;
    let t = &buf[t_at..];
    if &t[12..20] != END_MAGIC {
        return Err((t_at, "missing trailer (incomplete snapshot)".to_string()));
    }
    let body_len = u64::from_le_bytes(t[0..8].try_into().expect("8-byte slice"));
    if body_len != t_at as u64 {
        return Err((
            t_at,
            format!("trailer records {body_len} body bytes, file has {t_at}"),
        ));
    }
    let stored = crc32c_unmask(u32::from_le_bytes(
        t[8..12].try_into().expect("4-byte slice"),
    ));
    let body = &buf[..t_at];
    let actual = crc32c(body);
    if stored != actual {
        return Err((
            t_at,
            format!("checksum mismatch (stored {stored:08x}, computed {actual:08x})"),
        ));
    }
    if &body[0..8] != MAGIC {
        return Err((0, "not a belay snapshot (bad magic)".to_string()));
    }
    let version = u16::from_le_bytes([body[8], body[9]]);
    if version != VERSION {
        return Err((8, format!("unsupported snapshot version {version}")));
    }
    let (seq, count) = peek_header(body).expect("magic and length checked above");

    let mut dec = Decoder::new(&body[SNAPSHOT_HEADER_LEN..]);
    let at = |dec: &Decoder<'_>| SNAPSHOT_HEADER_LEN + dec.position();
    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for i in 0..count {
        let key = dec
            .get_bytes()
            .map_err(|e| (SNAPSHOT_HEADER_LEN + e.offset(), format!("entry {i}: {e}")))?;
        if key.len() > MAX_KEY_LEN {
            return Err((at(&dec), format!("entry {i}: key of {} bytes", key.len())));
        }
        if let Some((prev, _)) = entries.last() {
            if key <= prev.as_slice() {
                return Err((at(&dec), format!("entry {i}: keys out of order")));
            }
        }
        let value = dec
            .get_bytes()
            .map_err(|e| (SNAPSHOT_HEADER_LEN + e.offset(), format!("entry {i}: {e}")))?;
        entries.push((key.to_vec(), value.to_vec()));
    }
    if !dec.is_empty() {
        return Err((
            at(&dec),
            format!(
                "{} bytes after the last of {count} entries",
                dec.remaining()
            ),
        ));
    }
    // Sorted input lets the map be bulk-built in linear time.
    Ok(Snapshot {
        seq,
        map: entries.into_iter().collect(),
    })
}

/// Path of the snapshot for `seq` in `dir`.
#[cfg(test)]
pub(crate) fn path_for(dir: &Path, seq: u64) -> std::path::PathBuf {
    dir.join(snapshot_name(seq))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::{Crash, MemFs};
    use std::path::PathBuf;

    fn sample() -> BTreeMap<Vec<u8>, Vec<u8>> {
        (0..50u32)
            .map(|i| {
                (
                    format!("key/{i:04}").into_bytes(),
                    vec![i as u8; (i as usize * 7) % 300],
                )
            })
            .chain([(Vec::new(), b"empty key".to_vec())])
            .collect()
    }

    fn setup() -> (MemFs, PathBuf) {
        let fs = MemFs::new();
        fs.create_dir_all(Path::new("db")).expect("mkdir");
        (fs, PathBuf::from("db"))
    }

    #[test]
    fn layout_matches_the_documentation() {
        let mut map = BTreeMap::new();
        map.insert(b"k".to_vec(), b"vv".to_vec());
        let buf = encode(9, &map);
        assert_eq!(&buf[0..8], b"BELAYSNP");
        assert_eq!(&buf[8..10], &[1, 0]);
        assert_eq!(&buf[12..20], &9u64.to_le_bytes());
        assert_eq!(&buf[20..28], &1u64.to_le_bytes());
        assert_eq!(&buf[28..33], &[1, b'k', 2, b'v', b'v']);
        assert_eq!(&buf[33..41], &33u64.to_le_bytes());
        assert_eq!(&buf[45..53], b"BELAYEND");
        assert_eq!(buf.len(), 53);
    }

    #[test]
    fn written_file_matches_the_encoding_and_reads_back() {
        let (fs, dir) = setup();
        let map = sample();
        let size = write(&fs, &dir, 42, &map).expect("write");
        let path = path_for(&dir, 42);
        let bytes = fs.read(&path).expect("read");
        assert_eq!(bytes, encode(42, &map));
        assert_eq!(size, bytes.len() as u64);
        let snap = read(&fs, &path).expect("decode");
        assert_eq!(snap.seq, 42);
        assert_eq!(snap.map, map);
        assert_eq!(peek_header(&bytes), Some((42, map.len() as u64)));
        // Only the final file is left behind.
        assert_eq!(fs.list(&dir).expect("list"), vec![snapshot_name(42)]);
    }

    #[test]
    fn empty_snapshots_are_valid() {
        let buf = encode(0, &BTreeMap::new());
        assert_eq!(buf.len(), SNAPSHOT_HEADER_LEN + SNAPSHOT_TRAILER_LEN);
        let snap = decode(&buf).expect("decode");
        assert_eq!(snap.seq, 0);
        assert!(snap.map.is_empty());
    }

    #[test]
    fn every_truncation_is_rejected() {
        let buf = encode(3, &sample());
        for cut in 0..buf.len() {
            assert!(decode(&buf[..cut]).is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn bit_flips_anywhere_are_rejected() {
        let buf = encode(3, &sample());
        for i in (0..buf.len()).step_by(7) {
            let mut bad = buf.clone();
            bad[i] ^= 0x10;
            assert!(decode(&bad).is_err(), "flip at {i}");
        }
    }

    /// Re-seals a hand-modified body with a correct trailer.
    fn reseal(mut body: Vec<u8>) -> Vec<u8> {
        let t = trailer(body.len() as u64, crc32c(&body));
        body.extend_from_slice(&t);
        body
    }

    #[test]
    fn structural_errors_are_reported_with_offsets() {
        let mut body = header(1, 2).to_vec();
        body.extend_from_slice(&[1, b'b', 0, 1, b'a', 0]);
        let (offset, reason) = decode(&reseal(body)).unwrap_err();
        assert_eq!(reason, "entry 1: keys out of order");
        assert_eq!(offset, SNAPSHOT_HEADER_LEN + 5);

        let mut body = header(1, 1).to_vec();
        body.extend_from_slice(&[1, b'a', 0, 7]);
        let (_, reason) = decode(&reseal(body)).unwrap_err();
        assert!(reason.contains("after the last of 1 entries"), "{reason}");

        let mut body = header(1, 3).to_vec();
        body.extend_from_slice(&[1, b'a', 0]);
        let (_, reason) = decode(&reseal(body)).unwrap_err();
        assert!(reason.starts_with("entry 1:"), "{reason}");

        let mut body = header(1, 0).to_vec();
        body[8] = 9;
        let (offset, reason) = decode(&reseal(body)).unwrap_err();
        assert_eq!(
            (offset, reason.as_str()),
            (8, "unsupported snapshot version 9")
        );

        let mut body = header(1, 0).to_vec();
        body[0] = b'X';
        assert!(decode(&reseal(body)).unwrap_err().1.contains("bad magic"));
    }

    #[test]
    fn read_reports_corruption_with_the_file_name() {
        let (fs, dir) = setup();
        write(&fs, &dir, 5, &sample()).expect("write");
        let path = path_for(&dir, 5);
        fs.overwrite(&path, 40, b"??").expect("damage");
        let err = read(&fs, &path).unwrap_err();
        assert!(err.is_corruption(), "{err}");
        assert!(
            err.to_string().contains("snap-00000000000000000005.db"),
            "{err}"
        );
    }

    #[test]
    fn failed_writes_never_expose_a_partial_snapshot() {
        let (fs, dir) = setup();
        let map = sample();
        let path = path_for(&dir, 7);
        // Fail at each successive operation of the write, then crash.
        for budget in 0.. {
            let fs = fs.deep_copy();
            fs.fail_after_ops(Some(budget));
            let result = write(&fs, &dir, 7, &map);
            for crash in [Crash::ProcessExit, Crash::PowerLoss] {
                let after = fs.crash_copy(&crash);
                if after.exists(&path).expect("exists") {
                    let snap = read(&after, &path).expect("final name means complete");
                    assert_eq!(snap.map, map, "budget {budget}");
                } else {
                    assert!(result.is_err(), "budget {budget}: acknowledged but lost");
                }
            }
            if result.is_ok() {
                assert_eq!(fs.list(&dir).expect("list"), vec![snapshot_name(7)]);
                break;
            }
        }
    }

    #[test]
    fn a_snapshot_survives_power_loss_only_complete() {
        let (fs, dir) = setup();
        write(&fs, &dir, 1, &sample()).expect("write");
        let after = fs.crash_copy(&Crash::PowerLoss);
        let snap = read(&after, &path_for(&dir, 1)).expect("complete snapshot");
        assert_eq!(snap.map, sample());
    }
}
