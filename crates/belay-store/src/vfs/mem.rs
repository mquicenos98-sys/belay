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

impl State {
    /// Charges one mutating operation against the failure budget.
    fn charge(&mut self) -> io::Result<()> {
        match &mut self.op_budget {
            Some(0) => Err(io::Error::new(
                io::ErrorKind::Other,
                "simulated i/o failure (operation budget exhausted)",
            )),
            Some(n) => {
                *n -= 1;
                Ok(())
            }
            None => Ok(()),
        }
    }

    fn dir_exists(&self, dir: &Path) -> bool {
        dir.as_os_str().is_empty() || self.dirs.contains(dir)
    }

    fn require_parent(&self, path: &Path) -> io::Result<()> {
        let dir = parent(path);
        if self.dir_exists(dir) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{}: no such directory", dir.display()),
            ))
        }
    }

    fn file_id(&self, path: &Path) -> io::Result<FileId> {
        self.entries
            .get(path)
            .copied()
            .ok_or_else(|| not_found(path))
    }

    fn file(&self, path: &Path) -> io::Result<&FileData> {
        let id = self.file_id(path)?;
        Ok(&self.files[&id])
    }

    fn check_generation(&self, generation: u64) -> io::Result<()> {
        if generation == self.generation {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::Other,
                "file handle belongs to a process that crashed",
            ))
        }
    }

    /// The state a crash of kind `crash` leaves behind.
    fn after_crash(&self, crash: &Crash) -> State {
        let entries = match crash {
            Crash::ProcessExit => self.entries.clone(),
            Crash::PowerLoss | Crash::TornWrite { .. } => self.durable_entries.clone(),
        };
        let mut files = HashMap::new();
        for (path, id) in &entries {
            let file = &self.files[id];
            let keep = match crash {
                Crash::ProcessExit => file.data.len(),
                Crash::PowerLoss => file.synced,
                Crash::TornWrite { path: torn, keep } if torn == path => {
                    file.synced.saturating_add(*keep).min(file.data.len())
                }
                Crash::TornWrite { .. } => file.synced,
            };
            files.insert(
                *id,
                FileData {
                    data: file.data[..keep].to_vec(),
                    synced: keep,
                },
            );
        }
        State {
            generation: self.generation + 1,
            dirs: self.dirs.clone(),
            durable_entries: entries.clone(),
            entries,
            files,
            next_id: self.next_id,
            locks: BTreeSet::new(),
            fail_syncs: false,
            op_budget: None,
            stats: self.stats,
        }
    }
}

/// In-memory [`Vfs`] with crash simulation and fault injection.
///
/// Clones share the same file system. Each file keeps its contents and
/// the length known to be synced; directory entries are tracked as
/// visible and as of their directory's last sync. A simulated power loss
/// ([`Crash`]) keeps only synced data and synced entries, so a file created
/// or deleted without a directory sync disappears or comes back. Faults
/// can be injected with [`fail_syncs`](MemFs::fail_syncs) and
/// [`fail_after_ops`](MemFs::fail_after_ops).
///
/// ```
/// use belay_store::vfs::{Crash, MemFs, Vfs};
/// use std::io::Write;
/// use std::path::Path;
///
/// let fs = MemFs::new();
/// fs.create_dir_all(Path::new("db")).unwrap();
/// let path = Path::new("db").join("log");
/// let mut f = fs.create(&path).unwrap();
/// f.write_all(b"durable").unwrap();
/// f.sync().unwrap();
/// f.write_all(b" and not").unwrap();
/// fs.sync_dir(Path::new("db")).unwrap();
///
/// let after = fs.crash_copy(&Crash::PowerLoss);
/// assert_eq!(after.read(&path).unwrap(), b"durable");
/// let torn = fs.crash_copy(&Crash::TornWrite { path: path.clone(), keep: 4 });
/// assert_eq!(torn.read(&path).unwrap(), b"durable and");
/// ```
///
/// # Crashing in place
///
/// [`crash`](MemFs::crash) models the death of the process using the file
/// system. Clones taken before it (the ones handed to a store through
/// [`Options::fs`](crate::Options::fs)) belong to that process: from then
/// on their file handles, locks and mutating operations fail, the way a
/// dead process can no longer touch the disk. Handles made with
/// [`new`](MemFs::new), [`crash_copy`](MemFs::crash_copy) and
/// [`deep_copy`](MemFs::deep_copy), and clones taken after the crash, keep
/// working, so a test can inspect the aftermath and open the store again.
#[derive(Default)]
pub struct MemFs {
    state: Arc<Mutex<State>>,
    /// The generation this clone was taken in, or `None` for the test's own
    /// handles, which never go stale.
    process: Option<u64>,
}

impl Clone for MemFs {
    fn clone(&self) -> MemFs {
        MemFs {
            state: Arc::clone(&self.state),
            process: Some(lock(&self.state).generation),
        }
    }
}

impl MemFs {
    /// An empty file system.
    #[must_use]
    pub fn new() -> MemFs {
        MemFs::default()
    }

    fn from_state(state: State) -> MemFs {
        MemFs {
            state: Arc::new(Mutex::new(state)),
            process: None,
        }
    }

    /// Locks the state for a mutating operation, failing if this handle
    /// belongs to a process that crashed.
    fn mutable_state(&self) -> io::Result<MutexGuard<'_, State>> {
        let state = lock(&self.state);
        if let Some(generation) = self.process {
            state.check_generation(generation)?;
        }
        Ok(state)
    }

    /// Simulates a crash in place: the files shrink to what survives, and
    /// file handles, locks and clones from before the crash stop working.
    pub fn crash(&self, crash: &Crash) {
        let mut state = lock(&self.state);
        let mut next = state.after_crash(crash);
        next.stats.crashes += 1;
        *state = next;
    }

    /// An independent copy of the state a crash would leave, without
    /// disturbing this file system. Useful to try many crash points
    /// against the same history.
    #[must_use]
    pub fn crash_copy(&self, crash: &Crash) -> MemFs {
        MemFs::from_state(lock(&self.state).after_crash(crash))
    }

    /// An independent copy of the current state, unsynced data included
    /// (and still unsynced). Locks are not copied.
    #[must_use]
    pub fn deep_copy(&self) -> MemFs {
        let mut state = lock(&self.state).clone();
        state.locks.clear();
        state.generation += 1;
        MemFs::from_state(state)
    }

    /// How many leading bytes of the file at `path` are durable.
    pub fn synced_len(&self, path: &Path) -> io::Result<u64> {
        Ok(lock(&self.state).file(path)?.synced as u64)
    }

    /// Overwrites bytes of the file at `path` in place, synced region
    /// included, the way bit rot or a misdirected write would. Extends the
    /// file if needed.
    pub fn overwrite(&self, path: &Path, offset: usize, bytes: &[u8]) -> io::Result<()> {
        let mut state = lock(&self.state);
        let id = state.file_id(path)?;
        let file = state.files.get_mut(&id).expect("entries point at files");
        let end = offset + bytes.len();
        if file.data.len() < end {
            file.data.resize(end, 0);
        }
        file.data[offset..end].copy_from_slice(bytes);
        Ok(())
    }

    /// Makes every file sync fail (as after an I/O error on the device)
    /// until turned off.
    pub fn fail_syncs(&self, fail: bool) {
        lock(&self.state).fail_syncs = fail;
    }

    /// Lets `ops` more mutating operations (writes, syncs, creates,
    /// renames, removals, truncations, directory syncs) succeed, then fails
    /// all of them. `None` removes the limit.
    pub fn fail_after_ops(&self, ops: Option<u64>) {
        lock(&self.state).op_budget = ops;
    }

    /// Mutating operations left before injected failures begin, if limited.
    #[must_use]
    pub fn remaining_ops(&self) -> Option<u64> {
        lock(&self.state).op_budget
    }

    /// Operation counters.
    #[must_use]
    pub fn stats(&self) -> MemFsStats {
        lock(&self.state).stats
    }

    /// Every file path currently visible, sorted.
    #[must_use]
    pub fn paths(&self) -> Vec<PathBuf> {
        lock(&self.state).entries.keys().cloned().collect()
    }
}

impl fmt::Debug for MemFs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = lock(&self.state);
        f.debug_struct("MemFs")
            .field("files", &state.entries.len())
            .field("generation", &state.generation)
            .finish()
    }
}

struct MemFile {
    state: Arc<Mutex<State>>,
    id: FileId,
    generation: u64,
    path: PathBuf,
}

impl fmt::Debug for MemFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemFile").field("path", &self.path).finish()
    }
}

impl Write for MemFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut state = lock(&self.state);
        state.check_generation(self.generation)?;
        state.charge()?;
        state.stats.writes += 1;
        state.stats.bytes_written += buf.len() as u64;
        state
            .files
            .get_mut(&self.id)
            .expect("open handles keep their file")
            .data
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl VfsFile for MemFile {
    fn sync(&mut self) -> io::Result<()> {
        let mut state = lock(&self.state);
        state.check_generation(self.generation)?;
        if state.fail_syncs {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "simulated sync failure",
            ));
        }
        state.charge()?;
        state.stats.file_syncs += 1;
        let file = state
            .files
            .get_mut(&self.id)
            .expect("open handles keep their file");
        file.synced = file.data.len();
        Ok(())
    }
}

struct MemLock {
    state: Weak<Mutex<State>>,
    generation: u64,
    path: PathBuf,
}

impl fmt::Debug for MemLock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemLock").field("path", &self.path).finish()
    }
}

impl VfsLock for MemLock {}

impl Drop for MemLock {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            let mut state = lock(&state);
            // A crash already released every lock of the old generation.
            if state.generation == self.generation {
                state.locks.remove(&self.path);
            }
        }
    }
}
