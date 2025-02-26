//! The real file system.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use belay_common::fs::{sync_dir, LockFile};

use super::{Vfs, VfsFile, VfsLock};
use crate::error::Result;

/// [`Vfs`] backed by `std::fs`.
///
/// Syncing a file uses `sync_data` (fdatasync), which also persists the
/// file length. Directory syncs are real on Unix and a no-op on Windows,
/// where NTFS journals metadata changes itself. Locks are
/// [`LockFile`]s, with the stale-lock caveats documented there.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsFs;

impl OsFs {
    /// The real file system.
    #[must_use]
    pub fn new() -> OsFs {
        OsFs
    }
}

#[derive(Debug)]
struct OsFile {
    file: File,
}

impl Write for OsFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl VfsFile for OsFile {
    fn sync(&mut self) -> io::Result<()> {
        self.file.sync_data()
    }
}

#[derive(Debug)]
struct OsLock {
    _file: LockFile,
}

impl VfsLock for OsLock {}

impl Vfs for OsFs {
    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        fs::create_dir_all(dir)
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            if let Ok(name) = entry.file_name().into_string() {
                names.push(name);
            }
        }
        Ok(names)
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        path.try_exists()
    }

    fn file_len(&self, path: &Path) -> io::Result<u64> {
        Ok(fs::metadata(path)?.len())
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        fs::read(path)
    }

    fn create(&self, path: &Path) -> io::Result<Box<dyn VfsFile>> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        Ok(Box::new(OsFile { file }))
    }

    fn open_append(&self, path: &Path) -> io::Result<Box<dyn VfsFile>> {
        let file = OpenOptions::new().append(true).open(path)?;
        Ok(Box::new(OsFile { file }))
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        let file = OpenOptions::new().write(true).open(path)?;
        file.set_len(len)?;
        file.sync_all()
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        sync_dir(dir)
    }

    fn lock(&self, path: &Path) -> Result<Box<dyn VfsLock>> {
        let file = LockFile::acquire(path)?;
        Ok(Box::new(OsLock { _file: file }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StoreError;
    use belay_common::fs::TempDir;

    fn temp() -> TempDir {
        TempDir::with_prefix("belay-store-osfs").expect("temp dir")
    }

    #[test]
    fn create_append_read_and_truncate() {
        let dir = temp();
        let path = dir.join("a.log");
        let fs = OsFs::new();
        let mut f = fs.create(&path).expect("create");
        f.write_all(b"hello ").expect("write");
        f.sync().expect("sync");
        drop(f);
        let mut f = fs.open_append(&path).expect("append");
        f.write_all(b"world").expect("write");
        f.sync().expect("sync");
        drop(f);
        assert_eq!(fs.read(&path).expect("read"), b"hello world");
        assert_eq!(fs.file_len(&path).expect("len"), 11);
        fs.truncate(&path, 5).expect("truncate");
        assert_eq!(fs.read(&path).expect("read"), b"hello");
        // `create` truncates an existing file.
        drop(fs.create(&path).expect("recreate"));
        assert_eq!(fs.file_len(&path).expect("len"), 0);
    }

    #[test]
    fn list_skips_directories_and_rename_replaces() {
        let dir = temp();
        let fs = OsFs::new();
        fs.create_dir_all(&dir.join("sub").join("deeper"))
            .expect("mkdir");
        std::fs::write(dir.join("x"), b"1").expect("write");
        std::fs::write(dir.join("y"), b"2").expect("write");
        fs.rename(&dir.join("x"), &dir.join("y")).expect("rename");
        let mut names = fs.list(dir.path()).expect("list");
        names.sort();
        assert_eq!(names, vec!["y"]);
        assert_eq!(fs.read(&dir.join("y")).expect("read"), b"1");
        fs.remove(&dir.join("y")).expect("remove");
        assert!(!fs.exists(&dir.join("y")).expect("exists"));
        assert!(fs.exists(&dir.join("sub")).expect("exists"));
        fs.sync_dir(dir.path()).expect("sync dir");
    }

    #[test]
    fn open_append_requires_an_existing_file() {
        let dir = temp();
        let err = OsFs.open_append(&dir.join("missing")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn lock_is_exclusive_until_dropped() {
        let dir = temp();
        let path = dir.join("LOCK");
        let guard = OsFs.lock(&path).expect("first lock");
        match OsFs.lock(&path) {
            Err(StoreError::Locked { pid, .. }) => assert_eq!(pid, Some(std::process::id())),
            other => panic!("expected Locked, got {other:?}"),
        }
        drop(guard);
        assert!(!path.exists());
        drop(OsFs.lock(&path).expect("lock again"));
    }
}
