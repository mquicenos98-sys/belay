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
