//! Time-driven work: [`QueueEngine::tick`].
//!
//! The engine never starts threads of its own; whoever embeds it calls
//! `tick` periodically (belay-core does so a few times per second) with
//! the current time, and tests call it with a manual clock's time. One
//! tick, queue by queue:
//!
//! 1. promotes scheduled and retrying jobs whose `run_at` has come;
//! 2. expires leases that ran out: the attempt fails with a
//!    `lease expired` error (or `timed out` when the job's own timeout is
//!    what ended it), and the job retries, dies or is cancelled like any
//!    failed attempt;
//! 3. deletes finished jobs past their queue's retention, at most
//!    [`EngineOptions::retention_per_tick`](crate::EngineOptions::retention_per_tick)
//!    per tick so that a retention backlog is worked off gradually;
//! 4. releases timed uniqueness claims that ran out;
//! 5. reports workers that went silent.
//!
//! Each step works from the ordered indexes, so a tick with nothing due
//! costs a handful of lookups per queue. Work is written in batches of
//! [`EngineOptions::batch_size`](crate::EngineOptions::batch_size) with the
//! engine lock released in between.

use belay_common::{debug, format_duration, Timestamp};
use belay_proto::event::EventKind;
use belay_proto::{Job, JobError, QueueName};

use super::work::stage_failure;
use super::QueueEngine;
use crate::error::Result;
use crate::txn::{Ctx, Effects};

/// What one [`QueueEngine::tick`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Waiting jobs made ready.
    pub promoted: u64,
    /// Leases that ran out.
    pub expired: u64,
    /// Finished jobs deleted for retention.
    pub deleted: u64,
    /// Timed uniqueness claims released.
    pub claims_released: u64,
    /// Workers newly reported lost.
    pub workers_lost: u64,
    /// Retention stopped at its per-tick limit with more to delete; the
    /// next tick continues.
    pub backlog: bool,
}

impl TickReport {
    /// Whether the tick found nothing to do.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        *self == TickReport::default()
    }
}

impl QueueEngine {
    /// Runs every time-driven change due at `now`. See the module docs
    /// for the steps.
    pub fn tick(&self, now: Timestamp) -> Result<TickReport> {
        let mut report = TickReport::default();
        let batch = self.opts.batch_size;
        let names: Vec<QueueName> = self.lock().st.queues.keys().cloned().collect();

        for name in &names {
            loop {
                let (moved, effects) = {
                    let mut core = self.lock();
                    self.promote_due(&mut core, std::slice::from_ref(name), now, batch)?
                };
                self.after(effects);
                report.promoted += moved as u64;
                if moved < batch {
                    break;
                }
            }
        }

        for name in &names {
            loop {
                let expired = self.tick_step(now, |cx| expire_leases(cx, name, batch))?;
                report.expired += expired as u64;
                if expired < batch {
                    break;
                }
            }
        }

        let mut allowance = self.opts.retention_per_tick;
        'retention: for name in &names {
            loop {
                if allowance == 0 {
                    report.backlog = self.retention_pending(name, now);
                    break 'retention;
                }
                let limit = batch.min(allowance);
                let deleted = self.tick_step(now, |cx| enforce_retention(cx, name, limit))?;
                allowance -= deleted;
                report.deleted += deleted as u64;
                if deleted < limit {
                    break;
                }
            }
        }

        for name in &names {
            loop {
                let released = self.tick_step(now, |cx| release_claims(cx, name, batch))?;
                report.claims_released += released as u64;
                if released < batch {
                    break;
                }
            }
        }

        report.workers_lost = self.sweep_workers(now);
        if !report.is_idle() {
            debug!(
                "queue tick";
                promoted = report.promoted,
                expired = report.expired,
                deleted = report.deleted,
                claims_released = report.claims_released,
                workers_lost = report.workers_lost
            );
        }
        Ok(report)
    }

    /// One batch of tick work at `now`, committed under the lock.
    fn tick_step(
        &self,
        now: Timestamp,
        stage: impl FnOnce(&mut Ctx<'_>) -> Result<usize>,
    ) -> Result<usize> {
        let (count, effects): (usize, Effects) = {
            let mut core = self.lock();
            self.transact_locked(&mut core, now, stage)?
        };
        self.after(effects);
        Ok(count)
    }

    fn retention_pending(&self, queue: &QueueName, now: Timestamp) -> bool {
        self.lock()
            .st
            .queues
            .get(queue)
            .is_some_and(|q| !q.retention_due(now, 1).is_empty())
    }

    fn sweep_workers(&self, now: Timestamp) -> u64 {
        let mut core = self.lock();
        let lost = core
            .st
            .workers
            .sweep(now, self.opts.worker_timeout, self.opts.worker_retention);
        for (worker, last_seen) in &lost {
            self.sink.emit(EventKind::WorkerLost {
                worker: worker.clone(),
                last_seen: *last_seen,
            });
        }
        lost.len() as u64
    }
}

/// Fails the attempts of up to `limit` running jobs of `queue` whose lease
/// has run out.
fn expire_leases(cx: &mut Ctx<'_>, queue: &QueueName, limit: usize) -> Result<usize> {
    let Some(q) = cx.state().queues.get(queue) else {
        return Ok(0);
    };
    let mut expired = 0;
    for id in q.expired_leases(cx.now, limit) {
        let job = cx.require(id)?;
        if job.lease_expired(cx.now) {
            stage_expiry(cx, job)?;
            expired += 1;
        }
    }
    Ok(expired)
}

fn stage_expiry(cx: &mut Ctx<'_>, job: Job) -> Result<()> {
    let now = cx.now;
    let worker = job.lease.as_ref().map(|l| l.worker.clone());
    let timeout = job
        .deadline()
        .filter(|deadline| now >= *deadline)
        .and(job.spec.timeout);
    let mut error = match timeout {
        Some(timeout) => JobError::new(
            now,
            job.attempt,
            format!("timed out after {}", format_duration(timeout)),
        )
        .kind("timeout"),
        None => JobError::new(now, job.attempt, "lease expired").kind("lease_expired"),
    };
    error.worker = worker.clone();
    if let Some(worker) = worker {
        cx.emit(EventKind::JobExpired {
            job: job.id,
            queue: job.spec.queue.clone(),
            worker,
            attempt: job.attempt,
        });
    }
    cx.counters().expired += 1;
    stage_failure(cx, job, error, None)?;
    Ok(())
}

/// Deletes up to `limit` finished jobs of `queue` past their retention.
fn enforce_retention(cx: &mut Ctx<'_>, queue: &QueueName, limit: usize) -> Result<usize> {
    let Some(q) = cx.state().queues.get(queue) else {
        return Ok(0);
    };
    let ids = q.retention_due(cx.now, limit);
    for id in &ids {
        let job = cx.require(*id)?;
        cx.delete(&job);
        cx.emit(EventKind::JobDeleted {
            job: job.id,
            queue: job.spec.queue.clone(),
        });
    }
    cx.counters().deleted += ids.len() as u64;
    Ok(ids.len())
}

/// Releases up to `limit` timed claims of `queue` that ran out.
fn release_claims(cx: &mut Ctx<'_>, queue: &QueueName, limit: usize) -> Result<usize> {
    let Some(q) = cx.state().queues.get(queue) else {
        return Ok(0);
    };
    let keys = q.expired_claims(cx.now, limit);
    for key in &keys {
        cx.set_claim(queue, key, None);
    }
    Ok(keys.len())
}
