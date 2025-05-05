//! The worker protocol after leasing: heartbeat, complete, fail, and the
//! failure handling shared with the lease reaper.
//!
//! Every call names a job and presents the lease token handed out with
//! it. The call is refused with [`Error::LeaseLost`] unless the job is
//! running on that very token and the lease has not run out: after an
//! expiry the job may already be running elsewhere, and a late report from
//! the first worker must not overwrite the second's. A job that no longer
//! exists is also `LeaseLost`, since the worker's only sensible reaction
//! is the same: drop it.

use std::sync::atomic::Ordering;
use std::time::Duration;

use belay_proto::api::{
    ApiError, CompleteRequest, FailRequest, HeartbeatRequest, HeartbeatResponse, LeaseRequest,
    LeasedJob,
};
use belay_proto::event::EventKind;
use belay_proto::limits::{MAX_LEASE, MIN_LEASE};
use belay_proto::source::JobSource;
use belay_proto::{Job, JobError, JobId, JobState, LeaseToken, QueueName, Validate};

use super::QueueEngine;
use crate::error::{Error, Result};
use crate::queue::throughput::Metric;
use crate::txn::Ctx;
use crate::workers::Outcome;

impl QueueEngine {
    /// Renews the lease on a running job and records progress.
    ///
    /// The lease is extended to `extend_by` from now, or by its previous
    /// renewal length when not given, but never shortened and never past
    /// the attempt's deadline (`started_at + timeout`): a handler that
    /// overruns its timeout loses its lease there even if it keeps
    /// heartbeating. The response says whether cancellation was requested.
    pub fn heartbeat(&self, id: &JobId, req: &HeartbeatRequest) -> Result<HeartbeatResponse> {
        req.validate()?;
        self.worker_call(*id, req.token, |cx, mut job| {
            let now = cx.now;
            let Some(lease) = job.lease.as_mut() else {
                return Err(Error::LeaseLost(job.id));
            };
            let by = req.extend_by.unwrap_or_else(|| {
                lease
                    .expires_at
                    .saturating_duration_since(lease.heartbeat_at)
                    .clamp(MIN_LEASE, MAX_LEASE)
            });
            lease.renew(now, by);
            let response = HeartbeatResponse {
                expires_at: lease.expires_at,
                cancel_requested: job.cancel_requested,
            };
            if let Some(progress) = &req.progress {
                job.progress = Some(progress.clone());
                cx.emit(EventKind::JobProgress {
                    job: job.id,
                    queue: job.spec.queue.clone(),
                    progress: progress.clone(),
                });
            }
            job.updated_at = now;
            cx.counters().heartbeats += 1;
            cx.put(job);
            Ok(response)
        })
    }

    /// Records that the running attempt succeeded, with the handler's
    /// result. A job whose cancellation was requested still succeeds: the
    /// work is done.
    pub fn complete(&self, id: &JobId, req: &CompleteRequest) -> Result<()> {
        req.validate()?;
        self.worker_call(*id, req.token, |cx, mut job| {
            let queue = job.spec.queue.clone();
            let worker = job.lease.as_ref().map(|l| l.worker.clone());
            job.transition(JobState::Succeeded, cx.now)?;
            job.result = req.result.clone();
            cx.emit(EventKind::JobSucceeded {
                job: job.id,
                queue: queue.clone(),
                attempt: job.attempt,
                runtime: job.runtime().unwrap_or(Duration::ZERO),
            });
            cx.counters().succeeded += 1;
            cx.metric(&queue, Metric::Succeeded);
            if let Some(worker) = &worker {
                cx.outcome(worker, Outcome::Succeeded);
            }
            capacity_freed(cx, &queue);
            cx.finished(job.clone());
            cx.put(job);
            Ok(())
        })
    }

    /// Records that the running attempt failed. Returns where that took
    /// the job:
    ///
    /// - `cancelled` if cancellation had been requested;
    /// - `retrying` if `req.retryable` and attempts remain, eligible again
    ///   after `req.retry_in` or else the backoff delay of the job's policy
    ///   (the queue's default when the job has none);
    /// - `dead` otherwise.
    pub fn fail(&self, id: &JobId, req: &FailRequest) -> Result<JobState> {
        req.validate()?;
        self.worker_call(*id, req.token, |cx, job| {
            let worker = job.lease.as_ref().map(|l| l.worker.clone());
            let error = req.to_job_error(cx.now, job.attempt, worker);
            stage_failure(cx, job, error, req.retry_in)
        })
    }

    /// Runs `stage` on a job after checking that `token` holds its lease.
    fn worker_call<T>(
        &self,
        id: JobId,
        token: LeaseToken,
        stage: impl FnOnce(&mut Ctx<'_>, Job) -> Result<T>,
    ) -> Result<T> {
        let result = self.transact(|cx| {
            let held = cx
                .state()
                .jobs
                .get(&id)
                .is_some_and(|meta| meta.holds_lease(token, cx.now));
            if !held {
                return Err(Error::LeaseLost(id));
            }
            let job = cx.require(id)?;
            if job.lease.as_ref().map(|l| l.token) != Some(token) {
                return Err(Error::corrupt(
                    id.to_string(),
                    "stored lease token differs from the indexed one",
                ));
            }
            if let Some(lease) = &job.lease {
                cx.touch(&lease.worker);
            }
            stage(cx, job)
        });
        if matches!(result, Err(Error::LeaseLost(_))) {
            self.lease_lost.fetch_add(1, Ordering::Relaxed);
        }
        result
    }
}
