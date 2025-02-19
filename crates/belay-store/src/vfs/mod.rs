//! The handful of file operations the store performs, behind a trait.
//!
//! [`LogKv`](crate::LogKv) never touches `std::fs` directly. Production uses
//! [`OsFs`]; tests use [`MemFs`], which keeps files in memory and models what
//! survives a crash (only data that was synced, plus optionally a torn
//! prefix of what was not). That is what makes it practical to recover from
//! every possible truncation point of a log in a unit test.
//!
//! # Contract
//!
//! Implementations must provide these durability semantics, which the
//! store's recovery logic relies on:
//!
//! - Data written through a [`VfsFile`] is durable once
//!   [`VfsFile::sync`] returns. Unsynced data may be lost or torn by a crash.
//! - A new, renamed or removed directory entry is durable once
//!   [`Vfs::sync_dir`] returns for its parent directory.
//! - [`Vfs::truncate`] is durable when it returns.
//! - [`Vfs::rename`] atomically replaces the destination.
//! - [`Vfs::lock`] is exclusive across processes (for `OsFs`) or across
//!   handles sharing the same file system (for `MemFs`).

mod mem;

mod os;

use std::fmt;
use std::io::{self, Write};
use std::path::Path;

use crate::error::Result;

pub use mem::{Crash, MemFs, MemFsStats};
pub use os::OsFs;

/// File system operations used by the store. See the [module
/// docs](self) for the durability contract.
pub trait Vfs: Send + Sync + fmt::Debug {
    /// Creates `dir` and any missing parents.
    fn create_dir_all(&self, dir: &Path) -> io::Result<()>;

    /// Names of the regular files directly inside `dir`, in no particular
    /// order. Names that are not valid UTF-8 are skipped: the store never
    /// creates them.
    fn list(&self, dir: &Path) -> io::Result<Vec<String>>;

    /// Whether a file or directory exists at `path`.
    fn exists(&self, path: &Path) -> io::Result<bool>;

    /// Size of the file at `path` in bytes.
    fn file_len(&self, path: &Path) -> io::Result<u64>;

    /// The whole contents of the file at `path`.
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;

    /// Creates the file at `path`, truncating it if it exists, and opens
    /// it for writing.
    fn create(&self, path: &Path) -> io::Result<Box<dyn VfsFile>>;

    /// Opens an existing file for appending.
    fn open_append(&self, path: &Path) -> io::Result<Box<dyn VfsFile>>;

    /// Shortens (or extends with zeros) the file at `path` to `len` bytes
    /// and syncs it.
    fn truncate(&self, path: &Path, len: u64) -> io::Result<()>;

    /// Renames `from` to `to`, replacing `to` if it exists.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;

    /// Deletes the file at `path`.
    fn remove(&self, path: &Path) -> io::Result<()>;

    /// Makes creations, renames and removals inside `dir` durable.
    fn sync_dir(&self, dir: &Path) -> io::Result<()>;

    /// Takes an exclusive lock represented by the file at `path`, held
    /// until the returned guard is dropped.
    ///
    /// Fails with [`StoreError::Locked`](crate::StoreError::Locked) if it
    /// is already held.
    fn lock(&self, path: &Path) -> Result<Box<dyn VfsLock>>;
}

/// A file opened for writing by [`Vfs::create`] or [`Vfs::open_append`].
/// Writes always append.
pub trait VfsFile: io::Write + Send + fmt::Debug {
    /// Makes everything written so far durable.
    fn sync(&mut self) -> io::Result<()>;
}

/// A held lock from [`Vfs::lock`]; dropping it releases the lock.
pub trait VfsLock: Send + Sync + fmt::Debug {}

impl<T: Vfs + ?Sized> Vfs for std::sync::Arc<T> {
    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        (**self).create_dir_all(dir)
    }
    fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
        (**self).list(dir)
    }
    fn exists(&self, path: &Path) -> io::Result<bool> {
        (**self).exists(path)
    }
    fn file_len(&self, path: &Path) -> io::Result<u64> {
        (**self).file_len(path)
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        (**self).read(path)
    }
    fn create(&self, path: &Path) -> io::Result<Box<dyn VfsFile>> {
        (**self).create(path)
    }
    fn open_append(&self, path: &Path) -> io::Result<Box<dyn VfsFile>> {
        (**self).open_append(path)
    }
    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        (**self).truncate(path, len)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        (**self).rename(from, to)
    }
    fn remove(&self, path: &Path) -> io::Result<()> {
        (**self).remove(path)
    }
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        (**self).sync_dir(dir)
    }
    fn lock(&self, path: &Path) -> Result<Box<dyn VfsLock>> {
        (**self).lock(path)
    }
}
