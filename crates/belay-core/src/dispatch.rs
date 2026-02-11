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
