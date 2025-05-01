//! Operator actions on jobs (cancel, retry, promote, delete) and queues
//! (pause, resume, configure, purge, delete).
//!
//! Job actions are idempotent where that is unambiguous: cancelling a
//! cancelled job, promoting a ready one or pausing a paused queue succeed
//! without changing anything, so a retried HTTP request does no harm.
//! Actions that make no sense for the job's state fail with
//! [`Error::WrongState`], naming the states that would allow them.
//!
//! Bulk removals (`purge`, `delete_queue`) run in batches of
//! [`EngineOptions::batch_size`](crate::EngineOptions::batch_size) jobs,
//! releasing the engine lock between batches so that workers are not
//! stalled behind a large purge. They are therefore not atomic as a
//! whole; each batch is.

use std::collections::BTreeSet;
use std::ops::Bound;

use belay_common::text::plural;
use belay_proto::event::EventKind;
use belay_proto::{Job, JobId, JobState, QueueConfig, QueueInfo, QueueName, Validate};

use super::QueueEngine;
use crate::error::{Error, Result};
use crate::state::State;

/// Most index entries one purge batch looks at, so a purge of a rare
/// state in a huge queue still releases the lock regularly.
const SCAN_BUDGET_PER_BATCH: usize = 16;

impl QueueEngine {
    /// Cancels a job. Waiting and ready jobs are cancelled at once. A
    /// running job is marked `cancel_requested`: its worker learns this
    /// from its next heartbeat and should stop and report with `fail`,
    /// which then cancels it (as does its lease running out). Cancelling a
    /// cancelled job changes nothing. Returns the job as it now is.
    pub fn cancel(&self, id: &JobId) -> Result<Job> {
        self.transact(|cx| {
            let mut job = cx.require(*id)?;
            match job.state {
                JobState::Scheduled | JobState::Ready | JobState::Retrying => {
                    job.transition(JobState::Cancelled, cx.now)?;
                    cx.emit(EventKind::JobCancelled {
                        job: job.id,
                        queue: job.spec.queue.clone(),
                    });
                    cx.counters().cancelled += 1;
                    cx.finished(job.clone());
                    cx.put(job.clone());
                }
                JobState::Running if !job.cancel_requested => {
                    job.cancel_requested = true;
                    job.updated_at = cx.now;
                    cx.put(job.clone());
                }
                JobState::Running | JobState::Cancelled => {}
                JobState::Succeeded | JobState::Dead => {
                    return Err(Error::WrongState {
                        job: job.id,
                        state: job.state,
                        action: "cancel",
                        expected: "scheduled, ready, retrying or running",
                    });
                }
            }
            Ok(job.redacted())
        })
    }

    /// Sends a dead or cancelled job back to `ready` with a fresh budget of
    /// attempts (`attempt` restarts at 0; the error history is kept). Its
    /// uniqueness key is not claimed again.
    pub fn retry(&self, id: &JobId) -> Result<Job> {
        self.transact(|cx| {
            let mut job = cx.require(*id)?;
            if !matches!(job.state, JobState::Dead | JobState::Cancelled) {
                return Err(Error::WrongState {
                    job: job.id,
                    state: job.state,
                    action: "retry",
                    expected: "dead or cancelled",
                });
            }
            job.transition(JobState::Ready, cx.now)?;
            job.attempt = 0;
            job.run_at = cx.now;
            job.result = None;
            cx.emit(EventKind::JobRetried {
                job: job.id,
                queue: job.spec.queue.clone(),
            });
            cx.counters().retried += 1;
            cx.notify();
            cx.put(job.clone());
            Ok(job.redacted())
        })
    }

    /// Makes a scheduled or retrying job ready now. Promoting a ready job
    /// changes nothing.
    pub fn promote(&self, id: &JobId) -> Result<Job> {
        self.transact(|cx| {
            let mut job = cx.require(*id)?;
            match job.state {
                JobState::Ready => return Ok(job.redacted()),
                JobState::Scheduled | JobState::Retrying => {}
                state => {
                    return Err(Error::WrongState {
                        job: job.id,
                        state,
                        action: "promote",
                        expected: "scheduled or retrying",
                    });
                }
            }
            job.transition(JobState::Ready, cx.now)?;
            job.run_at = cx.now;
            cx.emit(EventKind::JobPromoted {
                job: job.id,
                queue: job.spec.queue.clone(),
            });
            cx.counters().promoted += 1;
            cx.notify();
            cx.put(job.clone());
            Ok(job.redacted())
        })
    }

    /// Deletes a finished (succeeded, dead or cancelled) job ahead of its
    /// retention. Pending jobs must be cancelled first.
    pub fn delete(&self, id: &JobId) -> Result<()> {
        self.transact(|cx| {
            let job = cx.require(*id)?;
            if !job.is_terminal() {
                return Err(Error::WrongState {
                    job: job.id,
                    state: job.state,
                    action: "delete",
                    expected: "succeeded, dead or cancelled",
                });
            }
            cx.delete(&job);
            cx.emit(EventKind::JobDeleted {
                job: job.id,
                queue: job.spec.queue.clone(),
            });
            cx.counters().deleted += 1;
            Ok(())
        })
    }

    /// Stops leasing from `queue` (enqueueing still works). Pausing a
    /// queue that does not exist yet creates it, so a queue can be paused
    /// before its first job arrives.
    pub fn pause(&self, queue: &QueueName) -> Result<()> {
        self.set_paused(queue, true)
    }

    /// Resumes leasing from `queue`.
    pub fn resume(&self, queue: &QueueName) -> Result<()> {
        self.set_paused(queue, false)
    }

    fn set_paused(&self, queue: &QueueName, paused: bool) -> Result<()> {
        self.transact(|cx| {
            let existed = cx.queue_record(queue).is_some();
            let mut record = cx.ensure_queue(queue);
            if existed && record.paused == paused {
                return Ok(());
            }
            record.paused = paused;
            cx.put_queue(queue, record);
            let queue = queue.clone();
            if paused {
                cx.emit(EventKind::QueuePaused { queue });
            } else {
                cx.emit(EventKind::QueueResumed { queue });
                cx.notify();
            }
            Ok(())
        })
    }

    /// Replaces the queue's configuration, creating the queue if needed.
    ///
    /// Concurrency, rate limit and retention apply at once. Defaults
    /// (`default_timeout`, `default_retry`, `default_max_attempts`) apply
    /// to jobs enqueued from now on; existing jobs keep the values filled
    /// in when they were enqueued, except that retries of jobs without a
    /// policy of their own use the new default backoff.
    pub fn set_config(&self, queue: &QueueName, config: QueueConfig) -> Result<QueueInfo> {
        config.validate()?;
        self.transact(|cx| {
            let mut record = cx.ensure_queue(queue);
            record.config = config;
            cx.put_queue(queue, record);
            cx.emit(EventKind::QueueConfigured {
                queue: queue.clone(),
            });
            // Limits may have been raised.
            cx.notify();
            Ok(())
        })?;
        self.queue_info(queue)
    }

    /// Deletes the jobs of `queue`, only those in `state` if given.
    /// Running jobs are never purged (cancel them instead), so `state`
    /// may not be `running`, and without a state every other job goes.
    /// Returns how many jobs were deleted.
    ///
    /// Purged jobs do not trigger `on_finished` hooks. A timed uniqueness
    /// claim held by a purged job stays until it runs out.
    pub fn purge(&self, queue: &QueueName, state: Option<JobState>) -> Result<u64> {
        if state == Some(JobState::Running) {
            return Err(Error::FailedPrecondition(
                "running jobs cannot be purged; cancel them instead".to_owned(),
            ));
        }
        let batch = self.opts.batch_size;
        let mut after: Option<JobId> = None;
        let mut purged = 0u64;
        loop {
            let (count, last, done) = self.transact(|cx| {
                let st = cx.state();
                let q = st
                    .queues
                    .get(queue)
                    .ok_or_else(|| Error::QueueNotFound(queue.clone()))?;
                let (ids, last, done) = select(&q.jobs, st, after, batch, |s| match state {
                    Some(wanted) => s == wanted,
                    None => s != JobState::Running,
                });
                for id in &ids {
                    let job = cx.require(*id)?;
                    cx.delete(&job);
                }
                cx.counters().deleted += ids.len() as u64;
                Ok((ids.len() as u64, last, done))
            })?;
            purged += count;
            if done {
                break;
            }
            after = last;
        }
        self.transact(|cx| {
            cx.emit(EventKind::QueuePurged {
                queue: queue.clone(),
                state,
                count: purged,
            });
            Ok(())
        })?;
        Ok(purged)
    }

    /// Deletes a queue with its configuration, uniqueness claims and jobs.
    ///
    /// Without `force`, a queue with pending (not finished) jobs is left
    /// alone with [`Error::FailedPrecondition`]; its finished jobs go with
    /// it. With `force`, everything goes, running jobs included: their
    /// workers get `lease_lost` on their next call. Returns how many jobs
    /// were deleted.
    pub fn delete_queue(&self, queue: &QueueName, force: bool) -> Result<u64> {
        let batch = self.opts.batch_size;
        let mut deleted = 0u64;
        loop {
            let (count, done) = self.transact(|cx| {
                let st = cx.state();
                let q = st
                    .queues
                    .get(queue)
                    .ok_or_else(|| Error::QueueNotFound(queue.clone()))?;
                let pending = q.counts.pending();
                if !force && pending > 0 {
                    return Err(Error::FailedPrecondition(format!(
                        "queue {queue} has {}; cancel or purge them, \
                         or delete the queue with force",
                        plural(pending, "pending job")
                    )));
                }
                let ids: Vec<JobId> = q.jobs.iter().take(batch).copied().collect();
                for id in &ids {
                    let job = cx.require(*id)?;
                    cx.delete(&job);
                }
                cx.counters().deleted += ids.len() as u64;
                let done = ids.len() == q.jobs.len();
                if done {
                    cx.drop_queue(queue);
                    cx.emit(EventKind::QueueDeleted {
                        queue: queue.clone(),
                    });
                }
                Ok((ids.len() as u64, done))
            })?;
            deleted += count;
            if done {
                return Ok(deleted);
            }
        }
    }
}

/// Picks up to `limit` ids from `ids` after `after` whose state passes
/// `wanted`, looking at a bounded number of entries. Returns the picks,
/// the last id looked at, and whether the end was reached.
fn select(
    ids: &BTreeSet<JobId>,
    st: &State,
    after: Option<JobId>,
    limit: usize,
    wanted: impl Fn(JobState) -> bool,
) -> (Vec<JobId>, Option<JobId>, bool) {
    let start = after.map_or(Bound::Unbounded, Bound::Excluded);
    let budget = limit.saturating_mul(SCAN_BUDGET_PER_BATCH);
    let mut picked = Vec::new();
    let mut last = after;
    let mut scanned = 0;
    for id in ids.range((start, Bound::Unbounded)) {
        last = Some(*id);
        if picked.len() == limit || scanned == budget {
            return (picked, last, false);
        }
        scanned += 1;
        if st.jobs.get(id).is_some_and(|meta| wanted(meta.state)) {
            picked.push(*id);
        }
    }
    (picked, last, true)
}
