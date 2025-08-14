//! An in-process [`Dispatcher`] that keeps jobs in a map.
//!
//! It models just enough of the queue for the engines to be exercised
//! without one: validation of submitted specs, unique keys with their
//! scopes and conflict policies, the start and end of a job's life, and
//! cancellation (immediate for waiting jobs, a request for running ones).
//! Tests drive jobs to completion by hand and can inject failures.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::sync::Mutex;

use belay_common::id::SyncUlidGenerator;
use belay_common::sync::lock;
use belay_common::{SharedClock, Timestamp};
use belay_json::Value;
use belay_proto::{
    Job, JobError, JobId, JobSpec, JobState, OnConflict, QueueName, Validate, WorkflowId,
};

use super::{DispatchError, DispatchOutcome, Dispatcher};

/// A dispatcher that holds jobs in memory, for tests and examples.
///
/// ```
/// use belay_common::ManualClock;
/// use belay_proto::{JobSpec, JobState, UniqueScope};
/// use belay_scheduler::{Dispatcher, MemoryDispatcher};
///
/// let clock = ManualClock::new("2025-06-01T00:00:00Z".parse().unwrap());
/// let queue = MemoryDispatcher::new(clock.shared());
/// let spec = JobSpec::new("reports", "build").unique("daily", UniqueScope::UntilFinished);
/// let first = queue.dispatch(&spec).unwrap();
/// let again = queue.dispatch(&spec).unwrap();
/// assert_eq!((again.id, again.duplicate), (first.id, true));
///
/// queue.succeed(&first.id, belay_json::json!({"rows": 12}));
/// assert_eq!(queue.job_state(&first.id), Some(JobState::Succeeded));
/// // The key was held until the job finished; now it is free again.
/// assert!(!queue.dispatch(&spec).unwrap().duplicate);
/// ```
pub struct MemoryDispatcher {
    clock: SharedClock,
    ids: SyncUlidGenerator,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    jobs: BTreeMap<JobId, Job>,
    /// Ids in the order they were created.
    order: Vec<JobId>,
    /// Unique key holders per queue.
    unique: HashMap<(QueueName, String), JobId>,
    cancel_requests: Vec<JobId>,
    /// Failures to return from the next dispatch calls, oldest first.
    failures: VecDeque<DispatchError>,
    dispatch_calls: u64,
}

impl MemoryDispatcher {
    /// An empty dispatcher that timestamps jobs with `clock`.
    #[must_use]
    pub fn new(clock: SharedClock) -> MemoryDispatcher {
        MemoryDispatcher {
            ids: SyncUlidGenerator::new(clock.clone()),
            clock,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Makes the next dispatch call fail with `error` (calls queue up).
    pub fn fail_next(&self, error: DispatchError) {
        lock(&self.inner).failures.push_back(error);
    }

    /// Every job created so far, oldest first.
    #[must_use]
    pub fn jobs(&self) -> Vec<Job> {
        let inner = lock(&self.inner);
        inner
            .order
            .iter()
            .filter_map(|id| inner.jobs.get(id).cloned())
            .collect()
    }

    /// Number of jobs created so far (duplicates not counted).
    #[must_use]
    pub fn job_count(&self) -> usize {
        lock(&self.inner).order.len()
    }

    /// Calls to [`dispatch`](Dispatcher::dispatch), including failed and
    /// duplicate ones.
    #[must_use]
    pub fn dispatch_calls(&self) -> u64 {
        lock(&self.inner).dispatch_calls
    }

    /// The job `id`, if it exists.
    #[must_use]
    pub fn get(&self, id: &JobId) -> Option<Job> {
        lock(&self.inner).jobs.get(id).cloned()
    }

    /// The most recently created job.
    #[must_use]
    pub fn last(&self) -> Option<Job> {
        let inner = lock(&self.inner);
        inner
            .order
            .last()
            .and_then(|id| inner.jobs.get(id).cloned())
    }

    /// The job dispatched for step `step` of workflow run `workflow`.
    #[must_use]
    pub fn find_step(&self, workflow: WorkflowId, step: &str) -> Option<Job> {
        lock(&self.inner)
            .jobs
            .values()
            .find(|job| {
                job.spec
                    .workflow
                    .as_ref()
                    .is_some_and(|w| w.id == workflow && w.step == step)
            })
            .cloned()
    }

    /// Running jobs whose cancellation was requested, in request order.
    #[must_use]
    pub fn cancel_requests(&self) -> Vec<JobId> {
        lock(&self.inner).cancel_requests.clone()
    }

    /// Moves a waiting job to `running`, as a worker's lease would.
    pub fn start(&self, id: &JobId) -> Option<Job> {
        let now = self.clock.now();
        let mut inner = lock(&self.inner);
        let job = inner.jobs.get_mut(id)?;
        run(job, now);
        Some(job.clone())
    }

    /// Completes job `id` with `result`, starting it first if needed.
    pub fn succeed(&self, id: &JobId, result: Value) -> Option<Job> {
        self.finish(id, JobState::Succeeded, Some(result), None)
    }

    /// Fails job `id` for good (`dead`) with `message`.
    pub fn fail(&self, id: &JobId, message: &str) -> Option<Job> {
        self.finish(id, JobState::Dead, None, Some(message))
    }

    /// Moves job `id` to terminal `state`, recording `result` or an error
    /// `message`. Returns the finished job, or `None` if it does not exist
    /// or has already finished.
    pub fn finish(
        &self,
        id: &JobId,
        state: JobState,
        result: Option<Value>,
        message: Option<&str>,
    ) -> Option<Job> {
        let now = self.clock.now();
        let mut inner = lock(&self.inner);
        let job = inner.jobs.get_mut(id)?;
        if job.is_terminal() || !state.is_terminal() {
            return None;
        }
        if state != JobState::Cancelled {
            run(job, now);
        }
        if let Some(message) = message {
            let error = JobError::new(now, job.attempt.max(1), message).retryable(false);
            job.record_error(error);
        }
        if job.transition(state, now).is_err() {
            // Waiting jobs can only be cancelled, not completed.
            return None;
        }
        job.result = result;
        Some(job.clone())
    }

    /// Removes job `id` as retention or a purge would.
    pub fn forget(&self, id: &JobId) -> Option<Job> {
        let mut inner = lock(&self.inner);
        let job = inner.jobs.remove(id)?;
        inner.order.retain(|x| x != id);
        inner.unique.retain(|_, holder| holder != id);
        Some(job)
    }
}
