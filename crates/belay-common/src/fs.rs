//! Filesystem helpers: crash-safe file replacement, self-cleaning scratch
//! directories and a lock file that keeps two processes off one data
//! directory.

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::rand::thread_rng;

/// Distinguishes temporary names created by one process.
static UNIQUE: AtomicU64 = AtomicU64::new(0);

/// The directory holding `path`, treating a bare file name as the current
/// directory.
fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

fn with_path_context(err: io::Error, action: &str, path: &Path) -> io::Error {
    io::Error::new(err.kind(), format!("{action} {}: {err}", path.display()))
}

/// Replaces the contents of `path` with `bytes` so that, even across a
/// crash or power loss, readers see either the old or the new contents in
/// full.
///
/// Writes a temporary file in the same directory, fsyncs it, renames it over
/// `path` and fsyncs the directory (see [`sync_dir`]). On error `path` is
/// untouched.
///
/// ```
/// use belay_common::fs::{atomic_write, TempDir};
///
/// let dir = TempDir::new().unwrap();
/// let path = dir.path().join("state.json");
/// atomic_write(&path, b"{\"v\":1}").unwrap();
/// atomic_write(&path, b"{\"v\":2}").unwrap();
/// assert_eq!(std::fs::read(&path).unwrap(), b"{\"v\":2}");
/// ```
pub fn atomic_write(path: impl AsRef<Path>, bytes: &[u8]) -> io::Result<()> {
    atomic_write_with(path, |file| file.write_all(bytes))
}

/// Like [`atomic_write`], with the contents produced by `write`, for
/// streaming large files such as snapshots without buffering them.
pub fn atomic_write_with<F>(path: impl AsRef<Path>, write: F) -> io::Result<()>
where
    F: FnOnce(&mut File) -> io::Result<()>,
{
    let path = path.as_ref();
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} does not name a file", path.display()),
        )
    })?;
    let dir = parent_dir(path);
    let mut tmp_name = OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(
        ".tmp-{}-{}",
        process::id(),
        UNIQUE.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = dir.join(tmp_name);

    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        write(&mut file)?;
        file.sync_all()?;
        // Windows cannot rename a file that is still open.
        drop(file);
        fs::rename(&tmp, path)?;
        sync_dir(dir)
    })();
    result.map_err(|e| with_path_context(e, "writing", path))
}

/// Flushes a directory's entries (new, renamed or deleted files) to disk,
/// which POSIX requires for a rename to survive a crash.
///
/// On platforms where a directory cannot be opened for syncing (Windows),
/// this does nothing; NTFS journals the rename itself.
pub fn sync_dir(dir: impl AsRef<Path>) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir.as_ref())?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// A uniquely named directory under the system temp dir, deleted with its
/// contents when dropped.
///
/// ```
/// use belay_common::fs::TempDir;
///
/// let path = {
///     let dir = TempDir::with_prefix("belay-doc").unwrap();
///     std::fs::write(dir.join("f"), b"x").unwrap();
///     dir.path().to_path_buf()
/// };
/// assert!(!path.exists());
/// ```
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
    keep: bool,
}

impl TempDir {
    /// A new directory named `belay-…` under [`std::env::temp_dir`].
    pub fn new() -> io::Result<TempDir> {
        TempDir::with_prefix("belay")
    }

    /// A new directory named `<prefix>-…` under [`std::env::temp_dir`].
    pub fn with_prefix(prefix: &str) -> io::Result<TempDir> {
        TempDir::new_in(std::env::temp_dir(), prefix)
    }

    /// A new directory named `<prefix>-…` inside `parent`.
    pub fn new_in(parent: impl AsRef<Path>, prefix: &str) -> io::Result<TempDir> {
        let parent = parent.as_ref();
        let mut rng = thread_rng();
        let mut last_err = None;
        // Collisions need the same pid, counter and 64 random bits; the
        // retries only guard against stale directories from a crashed run.
        for _ in 0..8 {
            let name = format!(
                "{prefix}-{}-{}-{:016x}",
                process::id(),
                UNIQUE.fetch_add(1, Ordering::Relaxed),
                rng.next_u64()
            );
            let path = parent.join(name);
            match fs::create_dir(&path) {
                Ok(()) => return Ok(TempDir { path, keep: false }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last_err = Some(e),
                Err(e) => return Err(with_path_context(e, "creating", &path)),
            }
        }
        Err(last_err.unwrap_or_else(|| io::Error::from(io::ErrorKind::AlreadyExists)))
    }

    /// The directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `path().join(child)`.
    #[must_use]
    pub fn join(&self, child: impl AsRef<Path>) -> PathBuf {
        self.path.join(child)
    }

    /// Keeps the directory on disk (e.g. to inspect a failed test) and
    /// returns its path.
    #[must_use]
    pub fn keep(mut self) -> PathBuf {
        self.keep = true;
        std::mem::take(&mut self.path)
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// An exclusive lock represented by a file that exists while it is held.
///
/// The file is created with `create_new`, which is atomic on every platform
/// belay supports, and records the holder's process id. Dropping the
/// `LockFile` (or [`release`](Self::release)) deletes it.
///
/// If a process dies without cleaning up, the file stays behind. Detecting
/// that is best-effort: on Linux a lock whose pid has no `/proc` entry is
/// taken over; elsewhere, or when the pid was reused by an unrelated
/// process, [`LockError::Held`] asks the operator to delete the file.
#[derive(Debug)]
pub struct LockFile {
    path: PathBuf,
    released: bool,
}

impl LockFile {
    /// Takes the lock at `path`, failing if another holder has it.
    ///
    /// ```
    /// use belay_common::fs::{LockFile, TempDir};
    ///
    /// let dir = TempDir::new().unwrap();
    /// let lock = LockFile::acquire(dir.join("LOCK")).unwrap();
    /// assert!(LockFile::acquire(dir.join("LOCK")).is_err());
    /// drop(lock);
    /// assert!(LockFile::acquire(dir.join("LOCK")).is_ok());
    /// ```
    pub fn acquire(path: impl AsRef<Path>) -> Result<LockFile, LockError> {
        let path = path.as_ref();
        let io_err = |source| LockError::Io {
            path: path.to_path_buf(),
            source,
        };
        let mut took_over = false;
        loop {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut file) => {
                    let written =
                        writeln!(file, "{}", process::id()).and_then(|()| file.sync_all());
                    return match written {
                        Ok(()) => Ok(LockFile {
                            path: path.to_path_buf(),
                            released: false,
                        }),
                        Err(e) => {
                            drop(file);
                            let _ = fs::remove_file(path);
                            Err(io_err(e))
                        }
                    };
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    let pid = read_pid(path);
                    if !took_over && pid.is_some_and(holder_is_gone) {
                        took_over = true;
                        match fs::remove_file(path) {
                            Ok(()) => continue,
                            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                            Err(e) => return Err(io_err(e)),
                        }
                    }
                    return Err(LockError::Held {
                        path: path.to_path_buf(),
                        pid,
                    });
                }
                Err(e) => return Err(io_err(e)),
            }
        }
    }

    /// The lock file's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Releases the lock, reporting a failure to delete the file (which
    /// dropping would ignore).
    pub fn release(mut self) -> io::Result<()> {
        self.released = true;
        fs::remove_file(&self.path)
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        if !self.released {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn read_pid(path: &Path) -> Option<u32> {
    let mut contents = String::new();
    File::open(path)
        .and_then(|f| f.take(64).read_to_string(&mut contents))
        .ok()?;
    contents.trim().parse().ok()
}

/// Whether the process that wrote a lock file is known to have exited.
fn holder_is_gone(pid: u32) -> bool {
    if pid == process::id() {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        Path::new("/proc/self").exists() && !Path::new("/proc").join(pid.to_string()).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}
