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
