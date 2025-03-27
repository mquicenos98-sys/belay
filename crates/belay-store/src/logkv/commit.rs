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

impl Inner {
    /// Commits `batch`, returning its sequence number once it is visible
    /// (and durable, under `SyncPolicy::Always`).
    pub(super) fn commit(&self, batch: WriteBatch) -> Result<u64> {
        batch.validate()?;
        self.ensure_open()?;
        if batch.is_empty() {
            return Ok(read(&self.visible).seq);
        }
        let payload = batch.encode();

        let mut st = lock(&self.commit);
        st.failed()?;
        // Checked under the commit lock: `close` sets the flag and then
        // waits on this lock for the queue to drain, so a writer admitted
        // here is always written before the log is closed.
        self.ensure_open()?;
        let seq = st.next_seq;
        st.next_seq += 1;
        st.queue.push(Pending {
            seq,
            payload,
            batch,
        });
        loop {
            let done = match self.sync {
                SyncPolicy::Always => st.durable_seq >= seq,
                SyncPolicy::Interval(_) | SyncPolicy::Never => st.applied_seq >= seq,
            };
            if done {
                return Ok(seq);
            }
            st.failed()?;
            if st.leader {
                st = wait(&self.committed, st);
                continue;
            }
            st.leader = true;
            let group = mem::take(&mut st.queue);
            drop(st);
            let outcome = self.write_group(group);
            st = lock(&self.commit);
            st.leader = false;
            match outcome {
                Ok(GroupOutcome { last, synced }) => {
                    st.applied_seq = last;
                    if synced {
                        st.durable_seq = st.durable_seq.max(last);
                    }
                }
                Err(e) => st.set_failure(&e),
            }
            self.committed.notify_all();
        }
    }

    /// Writes one group to the log and applies it. Runs with no other
    /// leader active.
    fn write_group(&self, group: Vec<Pending>) -> Result<GroupOutcome> {
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            return Ok(GroupOutcome {
                last: read(&self.visible).seq,
                synced: false,
            });
        };
        let (first, last) = (first.seq, last.seq);
        let mut wal = lock(&self.wal);
        let full = wal
            .active
            .as_ref()
            .is_some_and(|a| a.has_records() && a.len() >= self.segment_size);
        if full {
            self.roll(&mut wal, first)?;
        }
        let active = wal.active.as_mut().ok_or(StoreError::Closed)?;

        let size = group
            .iter()
            .map(|p| p.payload.len() + RECORD_HEADER_LEN)
            .sum();
        let mut framed = Vec::with_capacity(size);
        let mut ops = 0;
        for p in &group {
            active.frame(&mut framed, p.seq, &p.payload);
            ops += p.batch.len() as u64;
        }
        active.write(&framed, group.len() as u64)?;
        let synced = self.sync == SyncPolicy::Always;
        if synced && active.sync()? {
            self.counters.add(&self.counters.syncs, 1);
        }
        wal.written_seq = last;

        self.counters.add(&self.counters.groups, 1);
        self.counters
            .add(&self.counters.batches, group.len() as u64);
        self.counters.add(&self.counters.ops, ops);
        self.counters.add(&self.counters.bytes, framed.len() as u64);

        // Applied while still holding the log lock, so that whoever holds
        // it (a checkpoint) sees the table at exactly `written_seq`.
        {
            let mut visible = write(&self.visible);
            for p in group {
                visible.table.apply(p.batch);
            }
            visible.seq = last;
        }
        let live = wal.live_bytes();
        drop(wal);
        self.maybe_request_checkpoint(live);
        Ok(GroupOutcome { last, synced })
    }

    /// Seals the active segment and starts a new one whose first record
    /// will be `next_first`. The caller holds the log lock.
    pub(super) fn roll(&self, wal: &mut Wal, next_first: u64) -> Result<()> {
        let active = wal.active.as_mut().ok_or(StoreError::Closed)?;
        debug_assert!(
            active.has_records(),
            "rolling an empty segment would recreate it"
        );
        if active.sync()? {
            self.counters.add(&self.counters.syncs, 1);
        }
        let fresh = SegmentWriter::create(&*self.fs, &self.dir, next_first)?;
        let sealed = wal.active.replace(fresh).expect("checked above");
        debug!(
            "sealed log segment {}", sealed.path().display();
            bytes = sealed.len(),
            next_first
        );
        wal.seal(sealed.first_seq(), sealed.len());
        // Sealing synced everything written so far.
        self.mark_durable(wal.written_seq);
        Ok(())
    }

    /// Syncs the active segment, for `flush` and the interval flusher.
    pub(super) fn sync_wal(&self) -> Result<()> {
        lock(&self.commit).failed()?;
        let mut wal = lock(&self.wal);
        let seq = wal.written_seq;
        let active = wal.active.as_mut().ok_or(StoreError::Closed)?;
        match active.sync() {
            Ok(synced) => {
                if synced {
                    self.counters.add(&self.counters.syncs, 1);
                }
            }
            Err(e) => {
                drop(wal);
                self.fail(&e);
                return Err(e);
            }
        }
        drop(wal);
        self.mark_durable(seq);
        Ok(())
    }

    /// Records that everything up to `seq` is synced.
    pub(super) fn mark_durable(&self, seq: u64) {
        let mut st = lock(&self.commit);
        if seq > st.durable_seq {
            st.durable_seq = seq;
            self.committed.notify_all();
        }
    }

    /// Puts the store into the failed state.
    pub(super) fn fail(&self, err: &StoreError) {
        lock(&self.commit).set_failure(err);
        self.committed.notify_all();
    }

    /// The failure that stopped writes, if any.
    pub(super) fn failure(&self) -> Result<()> {
        lock(&self.commit).failed()
    }
}
