//! The seams between the engines: the scheduler and the workflow engine
//! dispatch jobs into the queue through [`QueueDispatcher`], and finished
//! queue jobs reach the workflow engine through [`WorkflowDriver`].
//!
//! # Re-entrancy
//!
//! The workflow engine holds its lock while it dispatches or cancels step
//! jobs. Cancelling a job that has not started finishes it on the spot,
//! and the queue runs its `on_finished` hooks on the calling thread, so a
//! hook that called straight back into the workflow engine would try to
//! take the lock its own thread already holds. [`WorkflowDriver`] breaks
//! the cycle: calls into the workflow engine are marked on the calling
//! thread, finished jobs reported during such a call are queued, and the
//! outermost call drains the queue once the workflow lock is released.
//! Jobs finished by other threads (workers completing, the queue tick
//! expiring leases) are handed over at once.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use belay_common::sync::lock;
use belay_common::warn;
use belay_proto::api::{ApiError, EnqueueStatus};
use belay_proto::{Job, JobId, JobSpec, JobState};
use belay_queue::QueueEngine;
use belay_scheduler::{DispatchError, DispatchOutcome, Dispatcher, WorkflowEngine};

/// Dispatches scheduler and workflow jobs into the local queue engine.
pub(crate) struct QueueDispatcher {
    queue: Arc<QueueEngine>,
}

impl QueueDispatcher {
    pub(crate) fn new(queue: Arc<QueueEngine>) -> QueueDispatcher {
        QueueDispatcher { queue }
    }
}

impl fmt::Debug for QueueDispatcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("QueueDispatcher")
    }
}

impl Dispatcher for QueueDispatcher {
    fn dispatch(&self, spec: &JobSpec) -> Result<DispatchOutcome, DispatchError> {
        // The queue honours the spec's unique key with its own conflict
        // policy; the engines set `return_existing`, which is what makes a
        // repeated dispatch after a crash come back as a duplicate.
        match self.queue.enqueue(spec.clone()) {
            Ok(result) => Ok(DispatchOutcome {
                id: result.id,
                duplicate: result.status == EnqueueStatus::Duplicate,
            }),
            Err(e) => Err(DispatchError::from(ApiError::from(e))),
        }
    }

    fn job_state(&self, id: &JobId) -> Option<JobState> {
        self.queue.job_state(id)
    }

    fn cancel(&self, id: &JobId) -> Result<(), DispatchError> {
        match self.queue.cancel(id) {
            Ok(_) => Ok(()),
            // Already finished or already gone: nothing left to cancel.
            Err(belay_queue::Error::WrongState { .. } | belay_queue::Error::JobNotFound(_)) => {
                Ok(())
            }
            Err(e) => Err(DispatchError::from(ApiError::from(e))),
        }
    }

    fn job(&self, id: &JobId) -> Option<Job> {
        self.queue.get(id).ok()
    }
}

thread_local! {
    /// Drivers with a call in progress on this thread, by id.
    static DRIVING: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

static NEXT_DRIVER_ID: AtomicUsize = AtomicUsize::new(1);

/// Marks a driver as busy on this thread for as long as it lives, so a
/// panic cannot leave the mark behind.
struct Driving {
    id: usize,
}

impl Driving {
    /// `None` if the driver is already busy on this thread.
    fn enter(id: usize) -> Option<Driving> {
        DRIVING.with(|d| {
            let mut d = d.borrow_mut();
            if d.contains(&id) {
                None
            } else {
                d.push(id);
                Some(Driving { id })
            }
        })
    }
}

impl Drop for Driving {
    fn drop(&mut self) {
        DRIVING.with(|d| d.borrow_mut().retain(|x| *x != self.id));
    }
}

/// Feeds finished jobs to the workflow engine without re-entering it; see
/// the module docs.
pub(crate) struct WorkflowDriver {
    id: usize,
    workflows: Arc<WorkflowEngine>,
    pending: Mutex<VecDeque<Job>>,
}

impl fmt::Debug for WorkflowDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkflowDriver")
            .field("pending", &self.pending())
            .finish_non_exhaustive()
    }
}

impl WorkflowDriver {
    pub(crate) fn new(workflows: Arc<WorkflowEngine>) -> WorkflowDriver {
        WorkflowDriver {
            id: NEXT_DRIVER_ID.fetch_add(1, Ordering::Relaxed),
            workflows,
            pending: Mutex::new(VecDeque::new()),
        }
    }

    pub(crate) fn engine(&self) -> &WorkflowEngine {
        &self.workflows
    }

    /// Runs `f` on the workflow engine, then hands it any jobs that
    /// finished meanwhile.
    pub(crate) fn call<R>(&self, f: impl FnOnce(&WorkflowEngine) -> R) -> R {
        let out = {
            let _busy = Driving::enter(self.id);
            f(&self.workflows)
        };
        self.drain();
        out
    }

    /// The queue's `on_finished` hook.
    pub(crate) fn job_finished(&self, job: &Job) {
        if job.spec.workflow.is_none() {
            return;
        }
        lock(&self.pending).push_back(job.clone());
        self.drain();
    }

    /// Reports queued jobs to the workflow engine, unless a call into it
    /// is in progress on this thread (that call drains when it returns).
    fn drain(&self) {
        let Some(_busy) = Driving::enter(self.id) else {
            return;
        };
        loop {
            let Some(job) = lock(&self.pending).pop_front() else {
                return;
            };
            if let Err(e) = self.workflows.on_job_finished(&job) {
                warn!("advancing a workflow failed; the periodic sweep will retry";
                    job = %job.id, error = %e);
            }
        }
    }

    /// Jobs waiting to be reported.
    pub(crate) fn pending(&self) -> usize {
        lock(&self.pending).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_common::{ManualClock, Timestamp};
    use belay_proto::source::NoopSink;
    use belay_proto::workflow::{StepSpec, WorkflowState};
    use belay_proto::{UniqueScope, WorkflowSpec};
    use belay_queue::EngineOptions;
    use belay_store::MemKv;

    fn queue() -> Arc<QueueEngine> {
        let clock = ManualClock::new(Timestamp::from_secs(1_700_000_000));
        Arc::new(
            QueueEngine::open(
                Arc::new(MemKv::new()),
                clock.shared(),
                NoopSink::shared(),
                EngineOptions::new().seed(1),
            )
            .unwrap(),
        )
    }

    #[test]
    fn repeated_unique_dispatches_come_back_as_duplicates() {
        let d = QueueDispatcher::new(queue());
        let spec = JobSpec::new("q", "k").unique("sched:x:1", UniqueScope::UntilFinished);
        let first = d.dispatch(&spec).unwrap();
        let again = d.dispatch(&spec).unwrap();
        assert!(!first.duplicate);
        assert_eq!((again.id, again.duplicate), (first.id, true));
        assert_eq!(d.job_state(&first.id), Some(JobState::Ready));
        assert_eq!(d.job(&first.id).map(|j| j.id), Some(first.id));
    }

    #[test]
    fn cancelling_finished_or_missing_jobs_is_not_an_error() {
        let d = QueueDispatcher::new(queue());
        let id = d.dispatch(&JobSpec::new("q", "k")).unwrap().id;
        d.cancel(&id).unwrap();
        assert_eq!(d.job_state(&id), Some(JobState::Cancelled));
        d.cancel(&id).unwrap();
        d.cancel(&JobId::generate()).unwrap();
    }

    #[test]
    fn invalid_specs_are_rejected_not_retried() {
        let d = QueueDispatcher::new(queue());
        let err = d
            .dispatch(&JobSpec::new("q", "k").priority(5000))
            .unwrap_err();
        assert!(!err.is_transient(), "{err}");
    }

    // Regression: cancelling a workflow cancelled its ready step jobs,
    // whose finished hook re-entered the workflow engine and deadlocked.
    #[test]
    fn jobs_finished_during_a_workflow_call_are_reported_after_it() {
        let q = queue();
        let clock = ManualClock::new(Timestamp::from_secs(1_700_000_000));
        let workflows = Arc::new(
            WorkflowEngine::open(
                Arc::new(MemKv::new()),
                clock.shared(),
                Arc::new(QueueDispatcher::new(Arc::clone(&q))),
                NoopSink::shared(),
            )
            .unwrap(),
        );
        let driver = Arc::new(WorkflowDriver::new(workflows));
        let hook = Arc::downgrade(&driver);
        q.on_finished(move |job| {
            if let Some(driver) = hook.upgrade() {
                driver.job_finished(job);
            }
        });
        let spec =
            WorkflowSpec::new("one-step").step(StepSpec::new("only", JobSpec::new("q", "k")));
        let run = driver.call(|w| w.submit(spec)).unwrap();
        let cancelled = driver.call(|w| w.cancel(run.id)).unwrap();
        assert_eq!(cancelled.state, WorkflowState::Cancelled);
        assert_eq!(driver.pending(), 0);
    }
}
