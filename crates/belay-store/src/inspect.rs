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
