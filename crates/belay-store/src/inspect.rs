//! Read-only inspection of a store directory, behind `belay admin
//! store-inspect` and `belay admin verify`.
//!
//! Nothing here takes the directory lock or modifies a file, so these
//! functions can look at a store that is open elsewhere. The active
//! segment of a running store may then show a torn tail that is simply a
//! write in progress.
//!
//! ```
//! use belay_store::inspect;
//! use belay_store::vfs::MemFs;
//! use belay_store::{Kv, LogKv, Options};
//! use std::path::Path;
//!
//! let fs = MemFs::new();
//! let kv = LogKv::open("db", Options::new().fs(fs.clone())).unwrap();
//! kv.put(b"k", b"v").unwrap();
//! kv.close().unwrap();
//!
//! let report = inspect::verify(&fs, Path::new("db")).unwrap();
//! assert!(report.is_ok());
//! assert_eq!(report.last_seq, 1);
//! ```

use std::fmt;
use std::path::{Path, PathBuf};

use belay_common::text::{humanize_bytes, Align, Table};

use crate::batch::{Op, WriteBatch};
use crate::error::{io_err, Result};
use crate::files;
use crate::keys::Printable;
use crate::snapshot;
use crate::vfs::Vfs;
use crate::wal::{parse_segment_header, Event, RecordReader, SEGMENT_HEADER_LEN};

/// A segment or snapshot file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileInfo {
    /// The file name.
    pub name: String,
    /// Full path.
    pub path: PathBuf,
    /// First sequence number (segments) or covered sequence number
    /// (snapshots), from the name.
    pub seq: u64,
    /// Size in bytes.
    pub size: u64,
}

/// The files of a store directory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoreFiles {
    /// The directory.
    pub dir: PathBuf,
    /// Log segments, oldest first.
    pub segments: Vec<FileInfo>,
    /// Snapshots, oldest first.
    pub snapshots: Vec<FileInfo>,
    /// Leftover temporary snapshots.
    pub temp_files: Vec<String>,
    /// Whether a `LOCK` file exists (the store is open, or its last user
    /// crashed).
    pub locked: bool,
    /// Files the store does not own, including snapshots set aside as
    /// `*.corrupt`.
    pub other_files: Vec<String>,
}

impl StoreFiles {
    /// Total size of the log segments.
    #[must_use]
    pub fn wal_bytes(&self) -> u64 {
        self.segments.iter().map(|f| f.size).sum()
    }
}

fn file_info(fs: &dyn Vfs, dir: &Path, seq: u64, name: &str) -> Result<FileInfo> {
    let path = dir.join(name);
    let size = fs.file_len(&path).map_err(io_err("reading", &path))?;
    Ok(FileInfo {
        name: name.to_string(),
        path,
        seq,
        size,
    })
}

/// Lists the files in the store directory `dir`.
pub fn list_files(fs: &dyn Vfs, dir: &Path) -> Result<StoreFiles> {
    let listing = files::list(fs, dir)?;
    let infos = |entries: &[(u64, String)]| -> Result<Vec<FileInfo>> {
        entries
            .iter()
            .map(|(seq, name)| file_info(fs, dir, *seq, name))
            .collect()
    };
    Ok(StoreFiles {
        dir: dir.to_path_buf(),
        segments: infos(&listing.segments)?,
        snapshots: infos(&listing.snapshots)?,
        temp_files: listing.temps,
        locked: listing.has_lock,
        other_files: listing.other,
    })
}

impl fmt::Display for StoreFiles {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut table = Table::new(["FILE", "KIND", "SEQ", "SIZE"])
            .align(2, Align::Right)
            .align(3, Align::Right);
        for s in &self.snapshots {
            table.push_row([
                s.name.clone(),
                "snapshot".into(),
                s.seq.to_string(),
                humanize_bytes(s.size),
            ]);
        }
        for s in &self.segments {
            table.push_row([
                s.name.clone(),
                "log".into(),
                s.seq.to_string(),
                humanize_bytes(s.size),
            ]);
        }
        for name in &self.temp_files {
            table.push_row([
                name.clone(),
                "temporary".into(),
                String::new(),
                String::new(),
            ]);
        }
        for name in &self.other_files {
            table.push_row([name.clone(), "other".into(), String::new(), String::new()]);
        }
        write!(f, "{table}")?;
        if self.locked {
            writeln!(
                f,
                "LOCK present: the store is open or was not closed cleanly"
            )?;
        }
        Ok(())
    }
}

/// What kind of problem [`verify`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProblemKind {
    /// Damage at the end of the last segment that a crash explains; opening
    /// the store truncates it.
    TornTail,
    /// Damage that a crash does not explain.
    Corrupt,
    /// Missing sequence numbers in the log.
    Gap,
    /// A snapshot that fails validation.
    BadSnapshot,
}

impl fmt::Display for ProblemKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ProblemKind::TornTail => "torn tail",
            ProblemKind::Corrupt => "corrupt",
            ProblemKind::Gap => "gap",
            ProblemKind::BadSnapshot => "bad snapshot",
        })
    }
}

/// One finding of [`verify`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// Its kind.
    pub kind: ProblemKind,
    /// The file.
    pub file: PathBuf,
    /// Byte offset within the file.
    pub offset: u64,
    /// Description.
    pub detail: String,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {} at offset {}: {}",
            self.kind,
            self.file.display(),
            self.offset,
            self.detail
        )
    }
}

/// One record of a log segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordInfo {
    /// Offset of the record header.
    pub offset: u64,
    /// Sequence number.
    pub seq: u64,
    /// The writer's known-synced offset when it wrote the record.
    pub synced_to: u64,
    /// Total size, header included.
    pub len: u64,
    /// The decoded batch; `None` if the payload does not decode.
    pub batch: Option<WriteBatch>,
}

impl RecordInfo {
    /// Offset just past this record.
    #[must_use]
    pub fn end(&self) -> u64 {
        self.offset + self.len
    }
}

impl fmt::Display for RecordInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "seq {} at {} ({} bytes, synced to {})",
            self.seq, self.offset, self.len, self.synced_to
        )?;
        let Some(batch) = &self.batch else {
            return f.write_str(": payload does not decode");
        };
        for op in batch.ops() {
            match op {
                Op::Put { key, value } => {
                    write!(f, "\n  put \"{}\" ({} bytes)", Printable(key), value.len())?
                }
                Op::Delete { key } => write!(f, "\n  delete \"{}\"", Printable(key))?,
                Op::DeleteRange {
                    start,
                    end: Some(end),
                } => write!(
                    f,
                    "\n  delete [\"{}\", \"{}\")",
                    Printable(start),
                    Printable(end)
                )?,
                Op::DeleteRange { start, end: None } => {
                    write!(f, "\n  delete [\"{}\", end)", Printable(start))?
                }
            }
        }
        Ok(())
    }
}

/// The decoded contents of one segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentDump {
    /// First sequence number from the header, if the header is valid.
    pub header_seq: Option<u64>,
    /// Valid records in file order.
    pub records: Vec<RecordInfo>,
    /// Damage found, classified as if this were the last segment.
    pub problems: Vec<Problem>,
}

/// Reads and decodes the segment at `path`. Damage is classified as it
/// would be in the last segment when `is_last` is set (torn tail or
/// corruption), and as corruption otherwise.
pub fn read_segment(fs: &dyn Vfs, path: &Path, is_last: bool) -> Result<SegmentDump> {
    let buf = fs.read(path).map_err(io_err("reading", path))?;
    let mut dump = SegmentDump {
        header_seq: None,
        records: Vec::new(),
        problems: Vec::new(),
    };
    let mut start = SEGMENT_HEADER_LEN;
    let problem = |kind, offset: usize, detail: String| Problem {
        kind,
        file: path.to_path_buf(),
        offset: offset as u64,
        detail,
    };
    let classify = |proven: bool| {
        if is_last && !proven {
            ProblemKind::TornTail
        } else {
            ProblemKind::Corrupt
        }
    };
    match parse_segment_header(&buf) {
        Ok(seq) => dump.header_seq = Some(seq),
        Err(reason) => {
            let damage = RecordReader::new(&buf, 0, 0).damage_at(0, reason);
            dump.problems
                .push(problem(classify(damage.proven), 0, damage.reason.clone()));
            start = damage.resume.unwrap_or(buf.len());
        }
    }
    let before = dump.header_seq.map_or(0, |s| s.saturating_sub(1));
    for event in RecordReader::new(&buf, start, before) {
        match event {
            Event::Record(rec) => dump.records.push(RecordInfo {
                offset: rec.offset as u64,
                seq: rec.seq,
                synced_to: rec.synced_to,
                len: (rec.end() - rec.offset) as u64,
                batch: WriteBatch::decode(rec.payload).ok(),
            }),
            Event::Damage(d) => {
                let detail = format!("{} ({} bytes)", d.reason, d.len_in(buf.len()));
                dump.problems
                    .push(problem(classify(d.proven), d.offset, detail));
            }
        }
    }
    Ok(dump)
}

/// The decoded contents of a snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotDump {
    /// Sequence number covered.
    pub seq: u64,
    /// Entries in key order.
    pub entries: Vec<(Vec<u8>, Vec<u8>)>,
}

/// Reads and validates the snapshot at `path`.
pub fn read_snapshot(fs: &dyn Vfs, path: &Path) -> Result<SnapshotDump> {
    let snap = snapshot::read(fs, path)?;
    Ok(SnapshotDump {
        seq: snap.seq,
        entries: snap.map.into_iter().collect(),
    })
}

/// Result of checking one snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotCheck {
    /// The file.
    pub file: FileInfo,
    /// Number of entries, if it validated.
    pub keys: Option<u64>,
}

/// Result of checking one segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentCheck {
    /// The file.
    pub file: FileInfo,
    /// Valid records.
    pub records: u64,
    /// Sequence numbers of the first and last valid record.
    pub seqs: Option<(u64, u64)>,
    /// Whether the newest valid snapshot covers the whole segment, so that
    /// opening the store would delete it.
    pub covered: bool,
}

/// Everything [`verify`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyReport {
    /// The files examined.
    pub files: StoreFiles,
    /// Every snapshot, oldest first.
    pub snapshots: Vec<SnapshotCheck>,
    /// Every segment, oldest first.
    pub segments: Vec<SegmentCheck>,
    /// Problems, in the order found.
    pub problems: Vec<Problem>,
    /// Sequence number of the newest valid snapshot (0 if none).
    pub snapshot_seq: u64,
    /// Last sequence number reachable from it through the log without a
    /// gap: what opening the store would recover if nothing were corrupt.
    pub last_seq: u64,
}

impl VerifyReport {
    /// Whether the store would open in strict mode without losing data: no
    /// problems other than a torn tail.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.problems
            .iter()
            .all(|p| p.kind == ProblemKind::TornTail)
    }

    /// Problems of the given kind.
    pub fn problems_of(&self, kind: ProblemKind) -> impl Iterator<Item = &Problem> {
        self.problems.iter().filter(move |p| p.kind == kind)
    }
}
