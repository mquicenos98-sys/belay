//! [`MemorySource`]: an in-memory [`JobSource`] for testing handlers and
//! workers without a server, with injectable failures and latency.
//!
//! It follows the lease protocol (tokens, attempt counting, `lease_lost`
//! for stale or revoked leases, long polling) but is not a queue engine:
//! jobs are ready as soon as they are pushed (`run_at` and `delay` are
//! ignored), retry delays are ignored, and nothing expires on its own —
//! use [`MemorySource::revoke_lease`] to simulate an expired lease.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use belay_common::rand::Rng;
use belay_common::sync::{lock, wait_timeout};
use belay_common::Timestamp;
use belay_json::Value;
use belay_proto::api::{
    ApiError, CompleteRequest, FailRequest, HeartbeatRequest, HeartbeatResponse, LeaseRequest,
    LeasedJob,
};
use belay_proto::source::JobSource;
use belay_proto::{Job, JobId, JobSpec, JobState, Lease, LeaseToken, Progress};

/// How an attempt ended, as reported to a [`MemorySource`].
#[derive(Clone, Debug, PartialEq)]
pub enum JobOutcome {
    /// `complete` with this result.
    Completed(Option<Value>),
    /// `fail` with this request.
    Failed(FailRequest),
}

impl JobOutcome {
    /// The failure, if this is one.
    #[must_use]
    pub fn failure(&self) -> Option<&FailRequest> {
        match self {
            JobOutcome::Failed(req) => Some(req),
            JobOutcome::Completed(_) => None,
        }
    }

    /// The result, if this is a completion (`Some(None)` for a completion
    /// without a result).
    #[must_use]
    pub fn result(&self) -> Option<Option<&Value>> {
        match self {
            JobOutcome::Completed(result) => Some(result.as_ref()),
            JobOutcome::Failed(_) => None,
        }
    }
}

#[derive(Default)]
struct Faults {
    down: Option<ApiError>,
    lease_failures: u32,
    heartbeat_failures: u32,
    report_failures: u32,
    latency: Duration,
    revoked: HashSet<JobId>,
    cancel: HashSet<JobId>,
}

struct Inner {
    ready: VecDeque<Job>,
    leased: HashMap<JobId, (Job, LeaseToken)>,
    outcomes: Vec<(JobId, JobOutcome)>,
    heartbeats: Vec<(JobId, HeartbeatRequest)>,
    lease_calls: usize,
    requeue: bool,
    faults: Faults,
    rng: Rng,
}

/// An in-memory job source. See the [module docs](self).
///
/// ```
/// use std::time::Duration;
/// use belay_proto::api::{CompleteRequest, LeaseRequest};
/// use belay_proto::source::JobSource;
/// use belay_proto::JobSpec;
/// use belay_worker::testing::{JobOutcome, MemorySource};
///
/// let source = MemorySource::new();
/// let id = source.push(JobSpec::new("default", "echo"));
/// let leased = source
///     .lease(&LeaseRequest::new("w1".parse()?, vec!["default".parse()?]))
///     .unwrap();
/// assert_eq!(leased[0].job.attempt, 1);
/// source.complete(&id, &CompleteRequest::new(leased[0].token)).unwrap();
/// assert_eq!(source.outcome(&id), Some(JobOutcome::Completed(None)));
/// # Ok::<(), belay_proto::ParseError>(())
/// ```
pub struct MemorySource {
    inner: Mutex<Inner>,
    changed: Condvar,
}

impl Default for MemorySource {
    fn default() -> MemorySource {
        MemorySource::new()
    }
}

impl MemorySource {
    /// An empty source that puts retryable failures back in line.
    #[must_use]
    pub fn new() -> MemorySource {
        MemorySource {
            inner: Mutex::new(Inner {
                ready: VecDeque::new(),
                leased: HashMap::new(),
                outcomes: Vec::new(),
                heartbeats: Vec::new(),
                lease_calls: 0,
                requeue: true,
                faults: Faults::default(),
                rng: Rng::from_entropy(),
            }),
            changed: Condvar::new(),
        }
    }

    /// Adds a job built from `spec`, ready at once.
    pub fn push(&self, spec: JobSpec) -> JobId {
        self.push_job(Job::new(JobId::generate(), spec, Timestamp::now()))
    }

    /// Adds `job` as is, made ready.
    pub fn push_job(&self, mut job: Job) -> JobId {
        let id = job.id;
        job.state = JobState::Ready;
        self.mutate(|s| s.ready.push_back(job));
        id
    }

    /// Whether a retryable failure with attempts left puts the job back in
    /// line (the default) or ends it.
    pub fn set_requeue(&self, requeue: bool) {
        self.mutate(|s| s.requeue = requeue);
    }

    /// Makes every call fail with `error` until called with `None`.
    pub fn set_down(&self, error: Option<ApiError>) {
        self.mutate(|s| s.faults.down = error);
    }

    /// Makes the next `n` lease calls fail with `unavailable`.
    pub fn fail_next_leases(&self, n: u32) {
        self.mutate(|s| s.faults.lease_failures = n);
    }

    /// Makes the next `n` heartbeat calls fail with `unavailable`.
    pub fn fail_next_heartbeats(&self, n: u32) {
        self.mutate(|s| s.faults.heartbeat_failures = n);
    }

    /// Makes the next `n` complete or fail calls fail with `unavailable`.
    pub fn fail_next_reports(&self, n: u32) {
        self.mutate(|s| s.faults.report_failures = n);
    }

    /// Delays every call by `d`.
    pub fn set_latency(&self, d: Duration) {
        self.mutate(|s| s.faults.latency = d);
    }

    /// Takes the lease on `id` away, as if it had expired and gone to
    /// another worker: every later call about it fails with `lease_lost`.
    pub fn revoke_lease(&self, id: &JobId) {
        self.mutate(|s| {
            s.leased.remove(id);
            s.faults.revoked.insert(*id);
        });
    }

    /// Marks `id` as cancel-requested; heartbeats say so from now on.
    pub fn request_cancel(&self, id: &JobId) {
        self.mutate(|s| {
            s.faults.cancel.insert(*id);
        });
    }

    /// Runs `f` on a consistent view of the source.
    pub fn view<R>(&self, f: impl FnOnce(&SourceView<'_>) -> R) -> R {
        f(&SourceView(&lock(&self.inner)))
    }

    /// Waits until `pred` holds, re-checking whenever the source changes.
    /// Returns `false` if `timeout` passes first.
    pub fn wait_until(
        &self,
        timeout: Duration,
        mut pred: impl FnMut(&SourceView<'_>) -> bool,
    ) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        let mut inner = lock(&self.inner);
        loop {
            if pred(&SourceView(&inner)) {
                return true;
            }
            let remaining = match deadline {
                Some(d) => d.saturating_duration_since(Instant::now()),
                None => Duration::from_secs(3600),
            };
            if remaining.is_zero() {
                return false;
            }
            inner = wait_timeout(&self.changed, inner, remaining).0;
        }
    }

    /// The latest outcome reported for `id`.
    #[must_use]
    pub fn outcome(&self, id: &JobId) -> Option<JobOutcome> {
        self.view(|v| v.outcome(id).cloned())
    }

    /// Every outcome reported, in order.
    #[must_use]
    pub fn outcomes(&self) -> Vec<(JobId, JobOutcome)> {
        lock(&self.inner).outcomes.clone()
    }

    fn mutate<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        let r = f(&mut lock(&self.inner));
        self.changed.notify_all();
        r
    }

    fn pause(&self) {
        let latency = lock(&self.inner).faults.latency;
        if !latency.is_zero() {
            thread::sleep(latency);
        }
    }
}

impl Inner {
    /// Hands out up to `req.max_jobs` jobs, queues in the order given,
    /// highest priority first within a queue, then oldest.
    fn take(&mut self, req: &LeaseRequest) -> Vec<LeasedJob> {
        let now = Timestamp::now();
        let mut out = Vec::new();
        for queue in &req.queues {
            while out.len() < req.max_jobs as usize {
                let best = self
                    .ready
                    .iter()
                    .enumerate()
                    .filter(|(_, j)| j.queue() == queue)
                    .max_by(|(ia, a), (ib, b)| {
                        a.spec.priority.cmp(&b.spec.priority).then(ib.cmp(ia))
                    })
                    .map(|(i, _)| i);
                let Some(mut job) = best.and_then(|i| self.ready.remove(i)) else {
                    break;
                };
                let token = LeaseToken::generate(&mut self.rng);
                job.attempt += 1;
                job.state = JobState::Running;
                job.started_at = Some(now);
                job.updated_at = now;
                job.lease = Some(Lease::new(req.worker.clone(), token, now, req.lease_for));
                self.leased.insert(job.id, (job.clone(), token));
                out.push(LeasedJob { job, token });
            }
        }
        out
    }

    /// The job leased under `token`, or `lease_lost`.
    fn held(&mut self, id: &JobId, token: LeaseToken) -> Result<&mut Job, ApiError> {
        if self.faults.revoked.contains(id) {
            return Err(ApiError::lease_lost(*id));
        }
        match self.leased.get_mut(id) {
            Some((job, held)) if *held == token => Ok(job),
            _ => Err(ApiError::lease_lost(*id)),
        }
    }

    fn check_down(&self) -> Result<(), ApiError> {
        match &self.faults.down {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    fn injected(counter: &mut u32, what: &str) -> Result<(), ApiError> {
        if *counter > 0 {
            *counter -= 1;
            return Err(ApiError::unavailable(format!("injected {what} failure")));
        }
        Ok(())
    }
}

impl JobSource for MemorySource {
    fn lease(&self, req: &LeaseRequest) -> Result<Vec<LeasedJob>, ApiError> {
        self.pause();
        let deadline = req.wait.and_then(|w| Instant::now().checked_add(w));
        let mut inner = lock(&self.inner);
        inner.lease_calls += 1;
        self.changed.notify_all();
        inner.check_down()?;
        Inner::injected(&mut inner.faults.lease_failures, "lease")?;
        loop {
            let jobs = inner.take(req);
            let remaining = deadline.map_or(Duration::ZERO, |d| {
                d.saturating_duration_since(Instant::now())
            });
            if !jobs.is_empty() || remaining.is_zero() {
                self.changed.notify_all();
                return Ok(jobs);
            }
            inner = wait_timeout(&self.changed, inner, remaining).0;
        }
    }

    fn heartbeat(&self, id: &JobId, req: &HeartbeatRequest) -> Result<HeartbeatResponse, ApiError> {
        self.pause();
        let mut inner = lock(&self.inner);
        inner.check_down()?;
        inner.held(id, req.token)?;
        Inner::injected(&mut inner.faults.heartbeat_failures, "heartbeat")?;
        let now = Timestamp::now();
        let job = inner.held(id, req.token)?;
        if let Some(p) = &req.progress {
            job.progress = Some(p.clone());
        }
        let lease_for = job.lease.as_ref().map_or(Duration::from_secs(30), |l| {
            l.expires_at.saturating_duration_since(l.acquired_at)
        });
        if let Some(lease) = &mut job.lease {
            lease.renew(now, req.extend_by.unwrap_or(lease_for));
        }
        let expires_at = job.lease.as_ref().map_or(now, |l| l.expires_at);
        let cancel_requested = inner.faults.cancel.contains(id);
        inner.heartbeats.push((*id, req.clone()));
        self.changed.notify_all();
        Ok(HeartbeatResponse {
            expires_at,
            cancel_requested,
        })
    }

    fn complete(&self, id: &JobId, req: &CompleteRequest) -> Result<(), ApiError> {
        self.pause();
        let mut inner = lock(&self.inner);
        inner.check_down()?;
        inner.held(id, req.token)?;
        Inner::injected(&mut inner.faults.report_failures, "report")?;
        inner.leased.remove(id);
        inner
            .outcomes
            .push((*id, JobOutcome::Completed(req.result.clone())));
        self.changed.notify_all();
        Ok(())
    }

    fn fail(&self, id: &JobId, req: &FailRequest) -> Result<(), ApiError> {
        self.pause();
        let mut inner = lock(&self.inner);
        inner.check_down()?;
        inner.held(id, req.token)?;
        Inner::injected(&mut inner.faults.report_failures, "report")?;
        let Some((mut job, _)) = inner.leased.remove(id) else {
            return Err(ApiError::lease_lost(*id));
        };
        inner.outcomes.push((*id, JobOutcome::Failed(req.clone())));
        let worker = job.lease.take().map(|l| l.worker);
        job.record_error(req.to_job_error(Timestamp::now(), job.attempt, worker));
        let cancelled = inner.faults.cancel.contains(id);
        if inner.requeue && req.retryable && !cancelled && job.attempts_left() > 0 {
            job.state = JobState::Ready;
            job.progress = None;
            inner.ready.push_back(job);
        }
        self.changed.notify_all();
        Ok(())
    }
}

impl fmt::Debug for MemorySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = lock(&self.inner);
        f.debug_struct("MemorySource")
            .field("ready", &inner.ready.len())
            .field("leased", &inner.leased.len())
            .field("outcomes", &inner.outcomes.len())
            .field("lease_calls", &inner.lease_calls)
            .finish()
    }
}
