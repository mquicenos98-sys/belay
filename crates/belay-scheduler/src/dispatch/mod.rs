//! The seam between this crate and the job queue.
//!
//! The scheduler and the workflow engine never enqueue jobs themselves:
//! they hand a finished [`JobSpec`] to a [`Dispatcher`]. belay-core
//! implements it over the queue engine; tests and examples use the
//! in-process [`MemoryDispatcher`].
//!
//! # Contract
//!
//! Both engines rely on the dispatcher honouring [`JobSpec::unique`] with
//! the `return_existing` conflict policy: every job they dispatch carries a
//! deterministic key (`sched:<id>:<fire ms>` for schedule fires,
//! `wf:<run>:<step>` for workflow steps), and dispatching the same key again
//! must return the job that already holds it with
//! [`DispatchOutcome::duplicate`] set. That is what makes a crash between
//! dispatching and saving the engine's own bookkeeping harmless: after a
//! restart the engine repeats the dispatch and gets the original job back.

mod memory;

pub use memory::MemoryDispatcher;

use std::fmt;
use std::sync::Arc;

use belay_proto::api::{ApiError, ErrorCode};
use belay_proto::{Job, JobId, JobSpec, JobState};

/// Where the scheduler and the workflow engine send the jobs they create.
///
/// Implementations are called while the engines hold their own locks, so
/// they must not call back into the engine that called them.
pub trait Dispatcher: Send + Sync {
    /// Enqueues `spec`, or returns the job already holding its unique key.
    fn dispatch(&self, spec: &JobSpec) -> Result<DispatchOutcome, DispatchError>;

    /// The current state of job `id`, or `None` if it does not exist (never
    /// did, or was deleted by retention).
    fn job_state(&self, id: &JobId) -> Option<JobState>;

    /// Asks for job `id` to be cancelled: at once if it has not started,
    /// through its worker's next heartbeat if it is running. Cancelling a
    /// job that already finished is not an error.
    fn cancel(&self, id: &JobId) -> Result<(), DispatchError>;

    /// The whole job record, including its result. The workflow engine
    /// uses it during [recovery](crate::WorkflowEngine::recover) to pick up
    /// results of steps that finished while it was not listening; the
    /// default says "unknown", in which case recovery learns only the final
    /// state and placeholders that refer to such a step fail.
    fn job(&self, id: &JobId) -> Option<Job> {
        let _ = id;
        None
    }
}

macro_rules! forward_dispatcher {
    ($($ptr:ty),*) => {$(
        impl<T: Dispatcher + ?Sized> Dispatcher for $ptr {
            fn dispatch(&self, spec: &JobSpec) -> Result<DispatchOutcome, DispatchError> {
                (**self).dispatch(spec)
            }

            fn job_state(&self, id: &JobId) -> Option<JobState> {
                (**self).job_state(id)
            }

            fn cancel(&self, id: &JobId) -> Result<(), DispatchError> {
                (**self).cancel(id)
            }

            fn job(&self, id: &JobId) -> Option<Job> {
                (**self).job(id)
            }
        }
    )*};
}

forward_dispatcher!(&T, Box<T>, Arc<T>);

/// What a successful [`Dispatcher::dispatch`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub struct DispatchOutcome {
    /// The job that now represents the request: new, or the existing holder
    /// of the spec's unique key.
    pub id: JobId,
    /// Whether an existing job was returned instead of a new one.
    pub duplicate: bool,
}

impl DispatchOutcome {
    /// A newly created job.
    pub fn created(id: JobId) -> DispatchOutcome {
        DispatchOutcome {
            id,
            duplicate: false,
        }
    }

    /// An existing job returned for a repeated unique key.
    pub fn duplicate(id: JobId) -> DispatchOutcome {
        DispatchOutcome {
            id,
            duplicate: true,
        }
    }
}

/// Why a dispatcher did not take a job.
///
/// The distinction matters to the engines: a rejected fire is lost (and
/// reported) because resubmitting the same spec cannot succeed, while an
/// unavailable dispatcher is retried later.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchError {
    /// The spec was refused and sending it again will not help: it breaks
    /// a rule, names a queue that cannot take jobs, or conflicts with a
    /// `reject` unique key.
    Rejected(String),
    /// The dispatcher cannot take jobs right now (shutting down, storage
    /// trouble, a remote queue out of reach); try again later.
    Unavailable(String),
}
