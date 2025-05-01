//! Handing jobs to workers: [`QueueEngine::lease`] and its long poll.
//!
//! # Which jobs a request gets
//!
//! 1. Due timers in the requested queues are promoted first, so a job whose
//!    `run_at` has passed is leasable even if `tick` has not run yet.
//! 2. Queues are visited according to [`QueueOrder`]: by default one pick
//!    per queue per round, starting from a position that rotates between
//!    requests, so no queue can starve another.
//! 3. Within a queue, tenants take turns by deficit round robin, and each
//!    tenant's jobs go by priority, then eligibility time, then id (see
//!    [`fair`](crate::queue::fair)).
//! 4. A queue gives nothing while paused, while `max_concurrency` of its
//!    jobs are running, or while its rate-limit bucket is empty.
//!
//! All picks of one request are planned against the committed state, then
//! written as one batch; the fairness cursors and token buckets only move
//! if that write succeeds.
//!
//! # Long polling
//!
//! With `wait`, a request that finds nothing sleeps on the engine's
//! [`Signal`](crate::signal::Signal) (never holding the engine lock) until
//! something that could produce work happens (an enqueue, a promotion, a
//! resume, capacity freed on a limited queue) or until the earliest timer
//! or rate-limit token it knows of, then looks again. It returns as soon
//! as it has at least one job, or empty when the wait runs out.

use std::time::{Duration, Instant};

use belay_common::sync::lock;
use belay_common::Timestamp;
use belay_proto::api::{LeaseRequest, LeasedJob};
use belay_proto::event::EventKind;
use belay_proto::{JobId, JobState, Lease, QueueName, Validate};

use super::QueueEngine;
use crate::error::{Error, Result};
use crate::options::{EngineOptions, QueueOrder};
use crate::queue::bucket::TokenBucket;
use crate::queue::fair::Planner;
use crate::queue::QueueState;
use crate::state::{Core, State};
use crate::stats::LeaseLatency;
use crate::txn::{Ctx, Effects, Plan};
use crate::workers::Outcome;

impl QueueEngine {
    /// Leases up to `req.max_jobs` jobs from `req.queues` to `req.worker`,
    /// long-polling up to `req.wait` when none is available.
    ///
    /// Each returned job is `running` with a fresh lease token that the
    /// worker presents to [`heartbeat`](Self::heartbeat),
    /// [`complete`](Self::complete) and [`fail`](Self::fail). The lease
    /// lasts `req.lease_for`, but never beyond the job's own `timeout`.
    /// Unknown queues are skipped, not an error: workers may start before
    /// anything has been enqueued.
    ///
    /// ```
    /// use std::sync::Arc;
    /// use belay_common::{ManualClock, Timestamp};
    /// use belay_proto::api::LeaseRequest;
    /// use belay_proto::source::NoopSink;
    /// use belay_proto::{JobSpec, JobState, QueueName, WorkerId};
    /// use belay_queue::{EngineOptions, QueueEngine};
    /// use belay_store::MemKv;
    ///
    /// let clock = ManualClock::new(Timestamp::from_secs(1_700_000_000));
    /// let engine = QueueEngine::open(Arc::new(MemKv::new()), clock.shared(), NoopSink::shared(), EngineOptions::new())?;
    /// engine.enqueue(JobSpec::new("emails", "send").priority(5))?;
    ///
    /// let req = LeaseRequest::new(WorkerId::from_static("w1"), vec![QueueName::from_static("emails")]);
    /// let leased = engine.lease(&req)?;
    /// assert_eq!(leased.len(), 1);
    /// assert_eq!((leased[0].job.state, leased[0].job.attempt), (JobState::Running, 1));
    /// assert!(engine.lease(&req)?.is_empty());
    /// # Ok::<(), belay_queue::Error>(())
    /// ```
    pub fn lease(&self, req: &LeaseRequest) -> Result<Vec<LeasedJob>> {
        req.validate()?;
        if self.is_shutting_down() {
            return Err(Error::ShuttingDown);
        }
        let started = Instant::now();
        let wait = req.wait.unwrap_or(Duration::ZERO);
        let mut first = true;
        loop {
            // Read before looking, so a change after the look is noticed.
            let seen = self.signal.current();
            let (jobs, hint) = self.lease_once(req, first)?;
            first = false;
            if !jobs.is_empty() {
                return Ok(jobs);
            }
            let elapsed = started.elapsed();
            if elapsed >= wait || self.is_shutting_down() {
                break;
            }
            let mut nap = (wait - elapsed).min(self.opts.max_poll_interval);
            if let Some(hint) = hint {
                nap = nap.min(hint.max(Duration::from_millis(1)));
            }
            self.waiting
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.signal.wait(seen, nap);
            self.waiting
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
        lock(&self.core).st.counters.empty_leases += 1;
        Ok(Vec::new())
    }

    /// One look at the queues: promote, plan, commit. Returns the leased
    /// jobs, or when there are none, how long until a timer or token might
    /// change that.
    fn lease_once(
        &self,
        req: &LeaseRequest,
        first: bool,
    ) -> Result<(Vec<LeasedJob>, Option<Duration>)> {
        let (jobs, hint, effects) = {
            let mut guard = self.lock();
            let core = &mut *guard;
            let now = self.clock.now();
            if first {
                core.st.counters.lease_requests += 1;
                if core.st.workers.seen(&req.worker, Some(&req.queues), now) {
                    self.sink.emit(EventKind::WorkerSeen {
                        worker: req.worker.clone(),
                        queues: req.queues.clone(),
                    });
                }
            } else {
                // Still waiting: the worker is alive.
                core.st.workers.seen(&req.worker, None, now);
            }
            let (_, mut effects) =
                self.promote_due(core, &req.queues, now, self.opts.batch_size)?;
            let ((jobs, hint), more) =
                self.transact_locked(core, now, |cx| stage_lease(cx, req, &self.opts))?;
            effects.merge(more);
            (jobs, hint, effects)
        };
        self.after(effects);
        Ok((jobs, hint))
    }

    /// Moves up to `limit` due scheduled and retrying jobs of `queues` to
    /// ready, earliest first, in one batch. Returns how many moved.
    pub(super) fn promote_due(
        &self,
        core: &mut Core,
        queues: &[QueueName],
        now: Timestamp,
        limit: usize,
    ) -> Result<(usize, Effects)> {
        let mut due: Vec<(Timestamp, JobId)> = Vec::new();
        for name in queues {
            if let Some(queue) = core.st.queues.get(name) {
                for id in queue.due_timers(now, limit) {
                    if let Some(meta) = core.st.jobs.get(&id) {
                        due.push((meta.run_at, id));
                    }
                }
            }
        }
        if due.is_empty() {
            return Ok((0, Effects::default()));
        }
        due.sort_unstable();
        due.truncate(limit);
        self.transact_locked(core, now, |cx| {
            let mut moved = 0;
            for (_, id) in &due {
                let mut job = cx.require(*id)?;
                if job.state.is_waiting() {
                    job.transition(JobState::Ready, cx.now)?;
                    cx.put(job);
                    moved += 1;
                }
            }
            if moved > 0 {
                cx.notify();
            }
            Ok(moved)
        })
    }
}

/// Picks from one queue within a lease plan: the fair planner plus copies
/// of the limits it consumes.
struct QueuePlan<'a> {
    queue: &'a QueueState,
    planner: Planner<'a>,
    bucket: Option<TokenBucket>,
    running: u64,
    picked: usize,
}

impl<'a> QueuePlan<'a> {
    fn new(queue: &'a QueueState) -> QueuePlan<'a> {
        QueuePlan {
            queue,
            planner: queue.ready.planner(),
            bucket: queue.bucket,
            running: queue.counts.running,
            picked: 0,
        }
    }

    fn next(&mut self, now: Timestamp, opts: &EngineOptions) -> Option<JobId> {
        if self.queue.paused || !self.planner.has_next() {
            return None;
        }
        if let Some(max) = self.queue.config.max_concurrency {
            if self.running >= u64::from(max) {
                return None;
            }
        }
        // Tokens are only taken once a job is known to be available, so an
        // idle queue does not drain its bucket.
        if let Some(bucket) = &mut self.bucket {
            if !bucket.try_take(now) {
                return None;
            }
        }
        let key = self
            .planner
            .next(|tenant| opts.tenant_quantum_for(tenant.as_ref()))?;
        self.running += 1;
        self.picked += 1;
        Some(key.id)
    }
}

/// Plans and stages one lease request. Returns the leased jobs, or the
/// wake hint when there are none.
fn stage_lease(
    cx: &mut Ctx<'_>,
    req: &LeaseRequest,
    opts: &EngineOptions,
) -> Result<(Vec<LeasedJob>, Option<Duration>)> {
    let st = cx.state();
    let now = cx.now;
    let mut plans: Vec<QueuePlan<'_>> = req
        .queues
        .iter()
        .filter_map(|name| st.queues.get(name))
        .map(QueuePlan::new)
        .collect();
    if plans.is_empty() {
        return Ok((Vec::new(), None));
    }

    let max = req.max_jobs as usize;
    let mut picks: Vec<JobId> = Vec::with_capacity(max);
    match opts.queue_order {
        QueueOrder::InOrder => {
            for plan in &mut plans {
                while picks.len() < max {
                    match plan.next(now, opts) {
                        Some(id) => picks.push(id),
                        None => break,
                    }
                }
            }
        }
        QueueOrder::RoundRobin => {
            let n = plans.len();
            let start = st.round_robin % n;
            let mut open: Vec<usize> = (0..n).map(|k| (start + k) % n).collect();
            while picks.len() < max && !open.is_empty() {
                open.retain(|&i| {
                    if picks.len() >= max {
                        return true;
                    }
                    match plans[i].next(now, opts) {
                        Some(id) => {
                            picks.push(id);
                            true
                        }
                        None => false,
                    }
                });
            }
        }
    }

    if picks.is_empty() {
        return Ok((Vec::new(), wake_hint(st, &req.queues, now)));
    }
    if opts.queue_order == QueueOrder::RoundRobin {
        cx.set_round_robin(st.round_robin.wrapping_add(1));
    }
    for plan in plans {
        if plan.picked > 0 {
            cx.plan(Plan {
                queue: plan.queue.name.clone(),
                cursor: plan.planner.into_cursor(),
                bucket: plan.bucket,
            });
        }
    }

    let mut leased = Vec::with_capacity(picks.len());
    for id in picks {
        let mut job = cx.require(id)?;
        job.transition(JobState::Running, now)?;
        job.attempt += 1;
        job.started_at = Some(now);
        let token = cx.token();
        let mut lease = Lease::new(req.worker.clone(), token, now, req.lease_for);
        if let Some(deadline) = job.deadline() {
            lease.expires_at = lease.expires_at.min(deadline);
        }
        job.lease = Some(lease);
        cx.latency(LeaseLatency {
            queue: job.spec.queue.clone(),
            waited: now.saturating_duration_since(job.run_at),
        });
        cx.emit(EventKind::JobStarted {
            job: id,
            queue: job.spec.queue.clone(),
            worker: req.worker.clone(),
            attempt: job.attempt,
        });
        cx.counters().leased += 1;
        cx.outcome(&req.worker, Outcome::Leased);
        cx.put(job.clone());
        leased.push(LeasedJob { job, token });
    }
    Ok((leased, None))
}
