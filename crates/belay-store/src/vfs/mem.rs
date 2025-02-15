//! An in-memory file system that can simulate crashes.
//!
//! Each file keeps its contents plus a *synced length*: the prefix known to
//! be on stable storage. Directory entries are tracked twice, as currently
//! visible and as of the last [`sync_dir`](Vfs::sync_dir) of their parent.
//! A simulated power loss keeps only the synced prefixes and the synced
//! directory entries, so a file created or deleted without a directory sync
//! disappears or comes back, as it can on a real disk.
//!
//! Simplifications, all on the pessimistic-but-plausible side: directories
//! themselves are durable as soon as they are created, and a file that
//! [`create`](Vfs::create) replaces is modelled as a fresh file (after a
//! crash the old contents may reappear under the old entry, never a mix).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use belay_common::sync::lock;

use super::{Vfs, VfsFile, VfsLock};
use crate::error::{Result, StoreError};

type FileId = u64;

/// How much of the written data survives a simulated crash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Crash {
    /// Power loss: only synced file data and synced directory entries
    /// survive.
    PowerLoss,
    /// The process dies but the operating system keeps running: everything
    /// written so far survives, synced or not.
    ProcessExit,
    /// Power loss during a write to `path`: like [`PowerLoss`](Crash::PowerLoss),
    /// except that the first `keep` bytes of that file's unsynced tail
    /// also made it to disk.
    TornWrite {
        /// The file that was being written.
        path: PathBuf,
        /// How many unsynced bytes survive (capped at what was written).
        keep: usize,
    },
}

/// Operation counters of a [`MemFs`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemFsStats {
    /// Calls to `write` on file handles.
    pub writes: u64,
    /// Bytes written through file handles.
    pub bytes_written: u64,
    /// File syncs.
    pub file_syncs: u64,
    /// Directory syncs.
    pub dir_syncs: u64,
    /// Simulated crashes applied in place.
    pub crashes: u64,
}

#[derive(Clone, Debug, Default)]
struct FileData {
    data: Vec<u8>,
    /// `data[..synced]` is durable.
    synced: usize,
}

#[derive(Clone, Debug, Default)]
struct State {
    /// Bumped by every in-place crash; handles from an older generation
    /// belong to the "dead process" and fail.
    generation: u64,
    dirs: BTreeSet<PathBuf>,
    entries: BTreeMap<PathBuf, FileId>,
    durable_entries: BTreeMap<PathBuf, FileId>,
    files: HashMap<FileId, FileData>,
    next_id: FileId,
    locks: BTreeSet<PathBuf>,
    fail_syncs: bool,
    /// Mutating operations still allowed before every one fails.
    op_budget: Option<u64>,
    stats: MemFsStats,
}

fn not_found(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("{}: no such file", path.display()),
    )
}

fn parent(path: &Path) -> &Path {
    path.parent().unwrap_or_else(|| Path::new(""))
}
