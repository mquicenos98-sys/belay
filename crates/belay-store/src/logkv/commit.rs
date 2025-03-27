//! The write path: group commit into the active log segment.
//!
//! Writers encode their batch outside any lock, then take a sequence number
//! and join a queue. Whichever writer finds no leader active becomes the
//! leader: it takes the *whole* queue, frames the batches into one buffer,
//! appends it with a single write, syncs once if the policy is `Always`,
//! applies the batches to the in-memory table and wakes everyone. Batches
//! that arrive meanwhile queue up for the next leader, so under load one
//! sync covers many writes and the cost of `Always` is amortized.
//!
//! Only one leader runs at a time, which keeps records in sequence order.
//! Batches become visible to readers only once written (and, under
//! `Always`, synced), so a reader never sees data a crash could take back.
//!
//! If a write or sync fails the store stops accepting writes: after a
//! failed fsync the kernel may have dropped the dirty pages, so retrying
//! could report success for data that is gone. Reopening recovers from
//! whatever reached the disk. A batch whose write failed may or may not
//! survive that reopen.

use std::mem;

use belay_common::debug;
use belay_common::sync::{lock, read, wait, write};

use super::{Inner, Wal};
use crate::batch::WriteBatch;
use crate::error::{Result, StoreError};
use crate::logkv::SyncPolicy;
use crate::wal::{SegmentWriter, RECORD_HEADER_LEN};

/// A batch waiting for a leader.
pub(super) struct Pending {
    seq: u64,
    payload: Vec<u8>,
    batch: WriteBatch,
}

/// Shared state of the commit queue.
#[derive(Default)]
pub(super) struct CommitState {
    /// Sequence number the next batch gets.
    pub(super) next_seq: u64,
    pub(super) queue: Vec<Pending>,
    /// Whether some writer is currently writing a group.
    pub(super) leader: bool,
    /// Highest sequence number written and visible.
    pub(super) applied_seq: u64,
    /// Highest sequence number known to be synced.
    pub(super) durable_seq: u64,
    /// Set once a write or sync fails; the store then refuses writes.
    pub(super) failure: Option<String>,
}

impl CommitState {
    /// State after recovering up to `last_seq`, all of which is durable.
    pub(super) fn recovered(last_seq: u64) -> CommitState {
        CommitState {
            next_seq: last_seq + 1,
            applied_seq: last_seq,
            durable_seq: last_seq,
            ..CommitState::default()
        }
    }

    fn failed(&self) -> Result<()> {
        match &self.failure {
            Some(reason) => Err(StoreError::Failed {
                reason: reason.clone(),
            }),
            None => Ok(()),
        }
    }

    pub(super) fn set_failure(&mut self, err: &StoreError) {
        if self.failure.is_none() {
            belay_common::error!("store stopped accepting writes"; error = %err);
            self.failure = Some(err.to_string());
        }
        // Queued writers learn about the failure when they wake up.
        self.queue.clear();
    }
}

struct GroupOutcome {
    last: u64,
    synced: bool,
}
