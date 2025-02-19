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

impl MemFs {
    fn handle(&self, state: &State, id: FileId, path: &Path) -> Box<dyn VfsFile> {
        Box::new(MemFile {
            state: Arc::clone(&self.state),
            id,
            generation: state.generation,
            path: path.to_path_buf(),
        })
    }
}

impl Vfs for MemFs {
    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        let mut state = self.mutable_state()?;
        state.charge()?;
        for ancestor in dir.ancestors() {
            if !ancestor.as_os_str().is_empty() {
                state.dirs.insert(ancestor.to_path_buf());
            }
        }
        Ok(())
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
        let state = lock(&self.state);
        if !state.dir_exists(dir) {
            return Err(not_found(dir));
        }
        Ok(state
            .entries
            .keys()
            .filter(|p| parent(p) == dir)
            .filter_map(|p| p.file_name()?.to_str().map(str::to_owned))
            .collect())
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        let state = lock(&self.state);
        Ok(state.entries.contains_key(path) || state.dirs.contains(path))
    }

    fn file_len(&self, path: &Path) -> io::Result<u64> {
        Ok(lock(&self.state).file(path)?.data.len() as u64)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        Ok(lock(&self.state).file(path)?.data.clone())
    }

    fn create(&self, path: &Path) -> io::Result<Box<dyn VfsFile>> {
        let mut state = self.mutable_state()?;
        state.require_parent(path)?;
        state.charge()?;
        let id = state.next_id;
        state.next_id += 1;
        state.files.insert(id, FileData::default());
        state.entries.insert(path.to_path_buf(), id);
        Ok(self.handle(&state, id, path))
    }

    fn open_append(&self, path: &Path) -> io::Result<Box<dyn VfsFile>> {
        let state = self.mutable_state()?;
        let id = state.file_id(path)?;
        Ok(self.handle(&state, id, path))
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        let mut state = self.mutable_state()?;
        let id = state.file_id(path)?;
        state.charge()?;
        let len = usize::try_from(len)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "length too large"))?;
        let file = state.files.get_mut(&id).expect("entries point at files");
        file.data.resize(len, 0);
        file.synced = len;
        Ok(())
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let mut state = self.mutable_state()?;
        let id = state.file_id(from)?;
        state.require_parent(to)?;
        state.charge()?;
        state.entries.remove(from);
        state.entries.insert(to.to_path_buf(), id);
        Ok(())
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        let mut state = self.mutable_state()?;
        state.file_id(path)?;
        state.charge()?;
        state.entries.remove(path);
        Ok(())
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        let mut state = self.mutable_state()?;
        if !state.dir_exists(dir) {
            return Err(not_found(dir));
        }
        state.charge()?;
        state.stats.dir_syncs += 1;
        let State {
            entries,
            durable_entries,
            ..
        } = &mut *state;
        durable_entries.retain(|p, _| parent(p) != dir);
        for (p, id) in entries.iter() {
            if parent(p) == dir {
                durable_entries.insert(p.clone(), *id);
            }
        }
        Ok(())
    }

    fn lock(&self, path: &Path) -> Result<Box<dyn VfsLock>> {
        let mut state = self
            .mutable_state()
            .map_err(|e| StoreError::io("locking", path, e))?;
        if !state.locks.insert(path.to_path_buf()) {
            return Err(StoreError::Locked {
                path: path.to_path_buf(),
                pid: None,
            });
        }
        Ok(Box::new(MemLock {
            state: Arc::downgrade(&self.state),
            generation: state.generation,
            path: path.to_path_buf(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (MemFs, PathBuf) {
        let fs = MemFs::new();
        fs.create_dir_all(Path::new("db")).expect("mkdir");
        (fs, PathBuf::from("db"))
    }

    fn write_file(fs: &MemFs, path: &Path, synced: &[u8], unsynced: &[u8]) {
        let mut f = fs.create(path).expect("create");
        f.write_all(synced).expect("write");
        f.sync().expect("sync");
        f.write_all(unsynced).expect("write");
    }

    #[test]
    fn power_loss_keeps_only_synced_data_and_entries() {
        let (fs, dir) = setup();
        let a = dir.join("a");
        let b = dir.join("b");
        write_file(&fs, &a, b"abc", b"def");
        fs.sync_dir(&dir).expect("sync dir");
        // `b` is never made durable in the directory.
        write_file(&fs, &b, b"123", b"");
        let after = fs.crash_copy(&Crash::PowerLoss);
        assert_eq!(after.read(&a).expect("read"), b"abc");
        assert!(!after.exists(&b).expect("exists"));
        assert_eq!(after.list(&dir).expect("list"), vec!["a"]);
        // The original is untouched.
        assert_eq!(fs.read(&a).expect("read"), b"abcdef");
    }

    #[test]
    fn process_exit_keeps_everything_written() {
        let (fs, dir) = setup();
        let a = dir.join("a");
        write_file(&fs, &a, b"x", b"yz");
        let after = fs.crash_copy(&Crash::ProcessExit);
        assert_eq!(after.read(&a).expect("read"), b"xyz");
        assert_eq!(after.synced_len(&a).expect("synced"), 3);
    }

    #[test]
    fn torn_write_keeps_a_prefix_of_the_unsynced_tail() {
        let (fs, dir) = setup();
        let a = dir.join("a");
        let b = dir.join("b");
        write_file(&fs, &a, b"head", b"tail");
        write_file(&fs, &b, b"B", b"unsynced");
        fs.sync_dir(&dir).expect("sync dir");
        for keep in 0..=6 {
            let after = fs.crash_copy(&Crash::TornWrite {
                path: a.clone(),
                keep,
            });
            let expected = &b"headtail"[..4 + keep.min(4)];
            assert_eq!(after.read(&a).expect("read"), expected, "keep={keep}");
            // Other files lose their unsynced tails as in a power loss.
            assert_eq!(after.read(&b).expect("read"), b"B");
        }
    }

    #[test]
    fn renames_and_removals_need_a_directory_sync() {
        let (fs, dir) = setup();
        let tmp = dir.join("snap.tmp");
        let fin = dir.join("snap");
        let old = dir.join("old");
        write_file(&fs, &old, b"old", b"");
        fs.sync_dir(&dir).expect("sync dir");
        write_file(&fs, &tmp, b"new", b"");
        fs.rename(&tmp, &fin).expect("rename");
        fs.remove(&old).expect("remove");
        let lost = fs.crash_copy(&Crash::PowerLoss);
        assert_eq!(lost.list(&dir).expect("list"), vec!["old"]);
        fs.sync_dir(&dir).expect("sync dir");
        let kept = fs.crash_copy(&Crash::PowerLoss);
        assert_eq!(kept.list(&dir).expect("list"), vec!["snap"]);
        assert_eq!(kept.read(&fin).expect("read"), b"new");
    }

    #[test]
    fn in_place_crash_invalidates_handles_and_locks() {
        let (fs, dir) = setup();
        let a = dir.join("a");
        let mut f = fs.create(&a).expect("create");
        f.write_all(b"1").expect("write");
        let guard = fs.lock(&dir.join("LOCK")).expect("lock");
        assert!(matches!(
            fs.lock(&dir.join("LOCK")),
            Err(StoreError::Locked { pid: None, .. })
        ));
        fs.crash(&Crash::ProcessExit);
        assert!(f.write_all(b"2").is_err());
        assert!(f.sync().is_err());
        // The lock died with the process; the new holder keeps it even when
        // the stale guard is dropped.
        let fresh = fs.lock(&dir.join("LOCK")).expect("lock after crash");
        drop(guard);
        assert!(fs.lock(&dir.join("LOCK")).is_err());
        drop(fresh);
        assert!(fs.lock(&dir.join("LOCK")).is_ok());
        assert_eq!(fs.read(&a).expect("read"), b"1");
        assert_eq!(fs.stats().crashes, 1);
    }

    #[test]
    fn clones_from_before_a_crash_belong_to_the_dead_process() {
        let (fs, dir) = setup();
        let process = fs.clone();
        drop(process.create(&dir.join("a")).expect("create"));
        fs.crash(&Crash::ProcessExit);
        // The crashed process can no longer change anything...
        assert!(process.create(&dir.join("b")).is_err());
        assert!(process.remove(&dir.join("a")).is_err());
        assert!(process.sync_dir(&dir).is_err());
        assert!(process.lock(&dir.join("LOCK")).is_err());
        // ...though reading is harmless.
        assert!(process.exists(&dir.join("a")).expect("exists"));
        // The test's handle and fresh clones keep working.
        let restarted = fs.clone();
        drop(restarted.create(&dir.join("b")).expect("create"));
        fs.remove(&dir.join("a")).expect("remove");
        assert_eq!(fs.list(&dir).expect("list"), vec!["b"]);
    }

    #[test]
    fn injected_failures() {
        let (fs, dir) = setup();
        let a = dir.join("a");
        let mut f = fs.create(&a).expect("create");
        fs.fail_syncs(true);
        f.write_all(b"data").expect("writes still work");
        assert!(f.sync().is_err());
        fs.fail_syncs(false);
        f.sync().expect("sync works again");

        fs.fail_after_ops(Some(2));
        f.write_all(b"1").expect("first op");
        f.sync().expect("second op");
        assert_eq!(fs.remaining_ops(), Some(0));
        assert!(f.write_all(b"2").is_err());
        assert!(fs.create(&dir.join("b")).is_err());
        assert!(fs.sync_dir(&dir).is_err());
        fs.fail_after_ops(None);
        f.write_all(b"3").expect("unlimited again");
        assert_eq!(fs.read(&a).expect("read"), b"data13");
    }

    #[test]
    fn overwrite_damages_durable_bytes() {
        let (fs, dir) = setup();
        let a = dir.join("a");
        write_file(&fs, &a, b"abcdef", b"");
        fs.sync_dir(&dir).expect("sync dir");
        fs.overwrite(&a, 2, b"XY").expect("overwrite");
        let after = fs.crash_copy(&Crash::PowerLoss);
        assert_eq!(after.read(&a).expect("read"), b"abXYef");
    }

    #[test]
    fn truncate_is_durable_and_can_extend() {
        let (fs, dir) = setup();
        let a = dir.join("a");
        write_file(&fs, &a, b"", b"abcdef");
        fs.sync_dir(&dir).expect("sync dir");
        fs.truncate(&a, 4).expect("truncate");
        assert_eq!(fs.synced_len(&a).expect("synced"), 4);
        let after = fs.crash_copy(&Crash::PowerLoss);
        assert_eq!(after.read(&a).expect("read"), b"abcd");
        fs.truncate(&a, 6).expect("extend");
        assert_eq!(fs.read(&a).expect("read"), b"abcd\0\0");
    }

    #[test]
    fn operations_on_missing_things_fail_with_not_found() {
        let fs = MemFs::new();
        let missing = Path::new("nope").join("f");
        assert_eq!(
            fs.create(&missing).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            fs.read(&missing).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            fs.open_append(&missing).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            fs.list(Path::new("nope")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            fs.remove(&missing).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        // Files at the root of a relative namespace need no directory.
        drop(fs.create(Path::new("top")).expect("create at root"));
        assert!(fs.exists(Path::new("top")).expect("exists"));
    }

    #[test]
    fn deep_copy_is_independent_and_keeps_unsynced_data() {
        let (fs, dir) = setup();
        let a = dir.join("a");
        write_file(&fs, &a, b"s", b"u");
        let copy = fs.deep_copy();
        let mut f = fs.open_append(&a).expect("append");
        f.write_all(b"more").expect("write");
        assert_eq!(copy.read(&a).expect("read"), b"su");
        assert_eq!(copy.synced_len(&a).expect("synced"), 1);
        assert_eq!(fs.paths(), vec![a.clone()]);
    }

    #[test]
    fn stats_count_writes_and_syncs() {
        let (fs, dir) = setup();
        write_file(&fs, &dir.join("a"), b"12", b"345");
        fs.sync_dir(&dir).expect("sync dir");
        let stats = fs.stats();
        assert_eq!(stats.writes, 2);
        assert_eq!(stats.bytes_written, 5);
        assert_eq!(stats.file_syncs, 1);
        assert_eq!(stats.dir_syncs, 1);
    }
}
