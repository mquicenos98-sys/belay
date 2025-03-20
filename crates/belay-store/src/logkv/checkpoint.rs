//! Checkpoints and the background thread that triggers them.
//!
//! A checkpoint runs in three phases:
//!
//! 1. **Freeze** (log lock held, writers wait): seal the active segment
//!    (sync it and start a new one), so that every segment on disk except
//!    the new one is covered by the state at this point, and clone the
//!    in-memory table.
//! 2. **Write** (no store locks held, writers proceed): stream the clone to
//!    `snap-<seq>.db.tmp`, sync it, rename it into place, sync the
//!    directory.
//! 3. **Trim**: delete the segments sealed in phase 1 and the previous
//!    snapshot.
//!
//! Writers therefore stall for one sync, one file creation and a copy of
//! the table, not for the snapshot write. The clone doubles memory for the
//! duration of the checkpoint, the price of keeping the whole dataset in a
//! plain `BTreeMap`.
//!
//! A crash at any point leaves a recoverable directory: before the rename,
//! the old snapshot plus the log still describe the state (the leftover
//! temporary file is deleted on open); after it, the new snapshot does, and
//! segments it covers are recognized by their sequence numbers and deleted
//! on open.

use std::io;
use std::mem;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use belay_common::sync::{lock, read, wait, wait_timeout};
use belay_common::{info, warn};

use super::{Inner, SyncPolicy};
use crate::error::{Result, StoreError};
use crate::files::{segment_name, snapshot_name};
use crate::kv::CheckpointInfo;
use crate::snapshot;

/// Coordination with the background thread.
#[derive(Debug, Default)]
pub(super) struct Background {
    pub(super) stop: bool,
    pub(super) checkpoint_requested: bool,
    pub(super) checkpoint_running: bool,
}

impl Inner {
    /// Writes a snapshot of the current state and discards the log it
    /// covers. Unless `force` is set, does nothing when no batch was
    /// written since the last checkpoint.
    pub(super) fn checkpoint(&self, force: bool) -> Result<CheckpointInfo> {
        self.ensure_open()?;
        self.failure()?;
        let _one_at_a_time = lock(&self.checkpoint_lock);
        let previous_seq = self.checkpoint_seq.load(Ordering::Acquire);
        let had_snapshot = self.snapshot_bytes.load(Ordering::Acquire) > 0;

        // Phase 1: freeze.
        let (seq, map, trim) = {
            let mut wal = lock(&self.wal);
            let seq = wal.written_seq;
            // Nothing written since the last snapshot (or ever).
            let unchanged = seq == previous_seq && (had_snapshot || seq == 0);
            if unchanged && !force {
                return Ok(CheckpointInfo {
                    seq,
                    keys: read(&self.visible).table.len() as u64,
                    snapshot_bytes: self.snapshot_bytes.load(Ordering::Acquire),
                    ..CheckpointInfo::default()
                });
            }
            let has_records = wal.active.as_ref().is_some_and(|a| a.has_records());
            if has_records {
                if let Err(e) = self.roll(&mut wal, seq + 1) {
                    self.fail(&e);
                    return Err(e);
                }
            }
            let trim: Vec<u64> = wal.sealed.keys().copied().collect();
            let visible = read(&self.visible);
            debug_assert_eq!(visible.seq, seq, "the table is applied under the log lock");
            (seq, visible.table.map().clone(), trim)
        };

        // Phase 2: write the snapshot, replacing the previous one.
        if had_snapshot && previous_seq != seq {
            self.remove_file(&snapshot_name(previous_seq));
        }
        let started = Instant::now();
        let bytes = snapshot::write(&*self.fs, &self.dir, seq, &map)?;

        // Phase 3: trim what the snapshot covers.
        {
            let mut wal = lock(&self.wal);
            for first in &trim {
                wal.forget(*first);
            }
        }
        let mut removed = 0u64;
        for first in &trim {
            if self.remove_file(&segment_name(*first)) {
                removed += 1;
            }
        }
        // The new snapshot is already durable; syncing the removals only
        // saves recovery from deleting the same files again.
        if let Err(e) = self.fs.sync_dir(&self.dir) {
            warn!("cannot sync store directory after a checkpoint: {e}");
        }

        self.checkpoint_seq.store(seq, Ordering::Release);
        self.snapshot_bytes.store(bytes, Ordering::Release);
        self.counters.add(&self.counters.checkpoints, 1);
        info!(
            "checkpoint written";
            seq,
            keys = map.len(),
            bytes,
            segments_removed = removed,
            elapsed = started.elapsed()
        );
        Ok(CheckpointInfo {
            seq,
            keys: map.len() as u64,
            snapshot_bytes: bytes,
            segments_removed: removed,
            written: true,
        })
    }

    /// Deletes a file in the store directory, logging failures other than
    /// "already gone". Returns whether the file was deleted.
    fn remove_file(&self, name: &str) -> bool {
        let path = self.dir.join(name);
        match self.fs.remove(&path) {
            Ok(()) => true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => {
                warn!("cannot remove {}: {e}", path.display());
                false
            }
        }
    }

    /// Asks the background thread for a checkpoint once the log on disk
    /// reaches the configured size.
    pub(super) fn maybe_request_checkpoint(&self, live_wal_bytes: u64) {
        let Some(limit) = self.checkpoint_bytes else {
            return;
        };
        if live_wal_bytes < limit {
            return;
        }
        let mut bg = lock(&self.bg);
        if !bg.stop && !bg.checkpoint_requested && !bg.checkpoint_running {
            bg.checkpoint_requested = true;
            self.bg_signal.notify_all();
        }
    }
}
