//! Names of the files in a store directory (see `crate::format` for the
//! layout) and sorting a directory listing into them.

use std::path::Path;

use crate::error::{io_err, Result};
use crate::vfs::Vfs;

/// Name of the lock file.
pub(crate) const LOCK_NAME: &str = "LOCK";

pub(crate) fn segment_name(first_seq: u64) -> String {
    format!("wal-{first_seq:020}.log")
}

pub(crate) fn snapshot_name(seq: u64) -> String {
    format!("snap-{seq:020}.db")
}

pub(crate) fn temp_snapshot_name(seq: u64) -> String {
    format!("snap-{seq:020}.db.tmp")
}

/// What a file name in a store directory denotes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FileKind {
    Segment(u64),
    Snapshot(u64),
    TempSnapshot,
    Lock,
    Other,
}

fn number(digits: &str) -> Option<u64> {
    if digits.len() == 20 && digits.bytes().all(|b| b.is_ascii_digit()) {
        digits.parse().ok()
    } else {
        None
    }
}

pub(crate) fn classify(name: &str) -> FileKind {
    if name == LOCK_NAME {
        return FileKind::Lock;
    }
    if let Some(seq) = name
        .strip_prefix("wal-")
        .and_then(|rest| rest.strip_suffix(".log"))
        .and_then(number)
    {
        return FileKind::Segment(seq);
    }
    if let Some(rest) = name.strip_prefix("snap-") {
        if let Some(seq) = rest.strip_suffix(".db").and_then(number) {
            return FileKind::Snapshot(seq);
        }
        if rest.strip_suffix(".db.tmp").and_then(number).is_some() {
            return FileKind::TempSnapshot;
        }
    }
    FileKind::Other
}

/// The store files found in a directory, each list sorted by number.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Listing {
    pub(crate) segments: Vec<(u64, String)>,
    pub(crate) snapshots: Vec<(u64, String)>,
    pub(crate) temps: Vec<String>,
    pub(crate) has_lock: bool,
    pub(crate) other: Vec<String>,
}

pub(crate) fn list(fs: &dyn Vfs, dir: &Path) -> Result<Listing> {
    let mut names = fs.list(dir).map_err(io_err("listing", dir))?;
    names.sort();
    let mut listing = Listing::default();
    for name in names {
        match classify(&name) {
            FileKind::Segment(seq) => listing.segments.push((seq, name)),
            FileKind::Snapshot(seq) => listing.snapshots.push((seq, name)),
            FileKind::TempSnapshot => listing.temps.push(name),
            FileKind::Lock => listing.has_lock = true,
            FileKind::Other => listing.other.push(name),
        }
    }
    // Zero padding makes name order numeric order already; sorting by the
    // number keeps that true even for hand-made files.
    listing.segments.sort();
    listing.snapshots.sort();
    Ok(listing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::MemFs;

    #[test]
    fn names_round_trip_through_classify() {
        assert_eq!(segment_name(1), "wal-00000000000000000001.log");
        assert_eq!(classify(&segment_name(1)), FileKind::Segment(1));
        assert_eq!(
            classify(&segment_name(u64::MAX)),
            FileKind::Segment(u64::MAX)
        );
        assert_eq!(classify(&snapshot_name(742)), FileKind::Snapshot(742));
        assert_eq!(classify(&temp_snapshot_name(742)), FileKind::TempSnapshot);
        assert_eq!(classify("LOCK"), FileKind::Lock);
    }

    #[test]
    fn near_misses_are_not_store_files() {
        for name in [
            "wal-1.log",
            "wal-0000000000000000000a.log",
            "wal-00000000000000000001.log.bak",
            "wal-99999999999999999999.log",
            "snap-00000000000000000001.dbx",
            "notes.txt",
            "lock",
        ] {
            assert_eq!(classify(name), FileKind::Other, "{name}");
        }
    }

    #[test]
    fn listing_sorts_and_groups_files() {
        let fs = MemFs::new();
        let dir = Path::new("db");
        fs.create_dir_all(dir).expect("mkdir");
        for name in [
            segment_name(30),
            segment_name(4),
            snapshot_name(29),
            temp_snapshot_name(40),
            "README".to_string(),
        ] {
            drop(fs.create(&dir.join(name)).expect("create"));
        }
        let listing = list(&fs, dir).expect("list");
        assert_eq!(
            listing.segments.iter().map(|s| s.0).collect::<Vec<_>>(),
            vec![4, 30]
        );
        assert_eq!(listing.snapshots, vec![(29, snapshot_name(29))]);
        assert_eq!(listing.temps, vec![temp_snapshot_name(40)]);
        assert_eq!(listing.other, vec!["README".to_string()]);
        assert!(!listing.has_lock);
    }
}
