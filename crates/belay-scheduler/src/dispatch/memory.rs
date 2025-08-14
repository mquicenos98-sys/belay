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

/// Walks a waiting job into `running`, counting the attempt.
fn run(job: &mut Job, now: Timestamp) {
    if job.state == JobState::Scheduled || job.state == JobState::Retrying {
        let _ = job.transition(JobState::Ready, now);
    }
    if job.state == JobState::Ready && job.transition(JobState::Running, now).is_ok() {
        job.attempt += 1;
        job.started_at = Some(now);
    }
}

impl Dispatcher for MemoryDispatcher {
    fn dispatch(&self, spec: &JobSpec) -> Result<DispatchOutcome, DispatchError> {
        let now = self.clock.now();
        let mut inner = lock(&self.inner);
        inner.dispatch_calls += 1;
        if let Some(error) = inner.failures.pop_front() {
            return Err(error);
        }
        spec.validate()
            .map_err(|v| DispatchError::Rejected(v.to_string()))?;
        if let Some(unique) = &spec.unique {
            let key = (spec.queue.clone(), unique.key.clone());
            let holder = inner.unique.get(&key).and_then(|id| inner.jobs.get(id));
            if let Some(holder) = holder {
                if unique.scope.holds(holder.state, holder.created_at, now) {
                    return match unique.on_conflict {
                        OnConflict::Reject => Err(DispatchError::Rejected(format!(
                            "unique key {:?} is held by job {}",
                            unique.key, holder.id
                        ))),
                        OnConflict::ReturnExisting | OnConflict::Replace => {
                            Ok(DispatchOutcome::duplicate(holder.id))
                        }
                    };
                }
            }
        }
        let id = JobId::from_ulid(
            self.ids
                .generate()
                .map_err(|e| DispatchError::Unavailable(e.to_string()))?,
        );
        if let Some(unique) = &spec.unique {
            inner
                .unique
                .insert((spec.queue.clone(), unique.key.clone()), id);
        }
        inner.jobs.insert(id, Job::new(id, spec.clone(), now));
        inner.order.push(id);
        Ok(DispatchOutcome::created(id))
    }

    fn job_state(&self, id: &JobId) -> Option<JobState> {
        lock(&self.inner).jobs.get(id).map(|job| job.state)
    }

    fn cancel(&self, id: &JobId) -> Result<(), DispatchError> {
        let now = self.clock.now();
        let mut inner = lock(&self.inner);
        let job = inner
            .jobs
            .get_mut(id)
            .ok_or_else(|| DispatchError::Rejected(format!("job {id} not found")))?;
        match job.state {
            s if s.is_terminal() => Ok(()),
            JobState::Running => {
                job.cancel_requested = true;
                inner.cancel_requests.push(*id);
                Ok(())
            }
            _ => job
                .transition(JobState::Cancelled, now)
                .map_err(|e| DispatchError::Rejected(e.to_string())),
        }
    }

    fn job(&self, id: &JobId) -> Option<Job> {
        self.get(id)
    }
}

impl fmt::Debug for MemoryDispatcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = lock(&self.inner);
        f.debug_struct("MemoryDispatcher")
            .field("jobs", &inner.jobs.len())
            .field("dispatch_calls", &inner.dispatch_calls)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_common::ManualClock;
    use belay_proto::{Unique, UniqueScope};
    use std::time::Duration;

    fn dispatcher() -> (ManualClock, MemoryDispatcher) {
        let clock = ManualClock::new(Timestamp::from_secs(1_750_000_000));
        let d = MemoryDispatcher::new(clock.shared());
        (clock, d)
    }

    #[test]
    fn timed_unique_keys_expire_after_their_period() {
        let (clock, d) = dispatcher();
        let spec = JobSpec::new("q", "k").unique("fire", UniqueScope::For(Duration::from_secs(60)));
        let first = d.dispatch(&spec).unwrap();
        d.succeed(&first.id, Value::Null);
        clock.advance(Duration::from_secs(59));
        assert!(
            d.dispatch(&spec).unwrap().duplicate,
            "finished but still held"
        );
        clock.advance(Duration::from_secs(1));
        assert!(!d.dispatch(&spec).unwrap().duplicate);
        assert_eq!(d.job_count(), 2);
        assert_eq!(d.dispatch_calls(), 3);
    }

    #[test]
    fn reject_policy_and_invalid_specs_are_refused() {
        let (_, d) = dispatcher();
        let spec = JobSpec::new("q", "k").unique_with(
            Unique::new("only-one", UniqueScope::UntilFinished).on_conflict(OnConflict::Reject),
        );
        let holder = d.dispatch(&spec).unwrap();
        let err = d.dispatch(&spec).unwrap_err();
        assert!(!err.is_transient());
        assert_eq!(
            err.message(),
            format!(r#"unique key "only-one" is held by job {}"#, holder.id)
        );
        let bad = JobSpec::new("q", "k").priority(5000);
        assert_eq!(
            d.dispatch(&bad).unwrap_err().to_string(),
            "rejected: $.priority: must be between -1000 and 1000, got 5000"
        );
    }

    #[test]
    fn injected_failures_come_first_and_are_used_once() {
        let (_, d) = dispatcher();
        d.fail_next(DispatchError::Unavailable("restarting".into()));
        let spec = JobSpec::new("q", "k");
        assert!(d.dispatch(&spec).unwrap_err().is_transient());
        assert!(d.dispatch(&spec).is_ok());
    }

    #[test]
    fn cancellation_is_immediate_unless_running() {
        let (_, d) = dispatcher();
        let waiting = d.dispatch(&JobSpec::new("q", "a")).unwrap().id;
        let running = d.dispatch(&JobSpec::new("q", "b")).unwrap().id;
        d.start(&running);
        d.cancel(&waiting).unwrap();
        d.cancel(&running).unwrap();
        assert_eq!(d.job_state(&waiting), Some(JobState::Cancelled));
        assert_eq!(d.job_state(&running), Some(JobState::Running));
        assert_eq!(d.cancel_requests(), [running]);
        assert!(d.get(&running).unwrap().cancel_requested);
        // Finished jobs cancel quietly; unknown ones do not.
        d.cancel(&waiting).unwrap();
        assert!(d.cancel(&JobId::generate()).is_err());
    }

    #[test]
    fn finishing_records_results_errors_and_only_happens_once() {
        let (_, d) = dispatcher();
        let a = d.dispatch(&JobSpec::new("q", "a")).unwrap().id;
        let b = d.dispatch(&JobSpec::new("q", "b")).unwrap().id;
        let done = d.succeed(&a, belay_json::json!({"ok": true})).unwrap();
        assert_eq!(done.attempt, 1);
        assert_eq!(done.result, Some(belay_json::json!({"ok": true})));
        assert!(d.succeed(&a, Value::Null).is_none(), "already finished");
        let dead = d.fail(&b, "boom").unwrap();
        assert_eq!(dead.state, JobState::Dead);
        assert_eq!(dead.last_error().unwrap().message, "boom");
        assert_eq!(d.forget(&a).unwrap().id, a);
        assert_eq!(d.job_state(&a), None);
        assert_eq!(d.jobs().len(), 1);
    }
}
