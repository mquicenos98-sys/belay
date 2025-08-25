//! How a workflow run moves: the rules of the
//! [`belay_proto::workflow`] module docs, applied to a [`WorkflowDetails`].
//!
//! The machine decides; an [`Effects`] implementation acts (dispatching
//! and cancelling jobs). Every change is recorded as an event and every
//! step result that a placeholder will read is collected, so that the
//! engine can persist and publish the outcome of one advancement at once.
//!
//! One advancement runs to a fixed point: finishing a step may make
//! dependents eligible, skipping those may make theirs eligible, and so on,
//! until every eligible step is running, deferred or finished.

use std::collections::BTreeSet;

use belay_common::Timestamp;
use belay_json::Value;
use belay_proto::event::EventKind;
use belay_proto::workflow::{StepFailurePolicy, StepSpec, StepState, WorkflowState};
use belay_proto::{Job, JobId, JobState, Workflow};

use super::record::WorkflowDetails;

/// How a step's job ended.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Outcome {
    /// The job succeeded. The result is `None` when it is unknown (the
    /// completion was recovered from the job's state alone); a handler that
    /// returned nothing has result `null`.
    Succeeded(Option<Value>),
    /// The job ended dead or cancelled, or never started; the message says
    /// why.
    Failed(String),
}

impl Outcome {
    /// The outcome of a finished job; `None` while it is still pending.
    pub(crate) fn of_job(job: &Job) -> Option<Outcome> {
        match job.state {
            JobState::Succeeded => Some(Outcome::Succeeded(Some(
                job.result.clone().unwrap_or(Value::Null),
            ))),
            JobState::Dead => {
                let reason = job
                    .last_error()
                    .map_or_else(|| "no error recorded".to_owned(), |e| e.message.clone());
                Some(Outcome::Failed(format!("job {} is dead: {reason}", job.id)))
            }
            JobState::Cancelled => Some(Outcome::Failed(format!("job {} was cancelled", job.id))),
            _ => None,
        }
    }

    /// The outcome known from a job's state alone.
    pub(crate) fn of_state(id: JobId, state: JobState) -> Option<Outcome> {
        match state {
            JobState::Succeeded => Some(Outcome::Succeeded(None)),
            JobState::Dead => Some(Outcome::Failed(format!("job {id} is dead"))),
            JobState::Cancelled => Some(Outcome::Failed(format!("job {id} was cancelled"))),
            _ => None,
        }
    }
}

/// What happened when a step's job was dispatched.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Start {
    /// The job is queued or running.
    Running(JobId),
    /// The dispatcher returned a job from an earlier attempt that has
    /// already finished.
    Finished(JobId, Outcome),
    /// The step cannot run: its payload did not resolve or the dispatcher
    /// rejected the job.
    Failed(String),
    /// The dispatcher is unavailable; the step stays pending and is retried
    /// by recovery.
    Deferred(String),
}

/// The machine's hands.
pub(crate) trait Effects {
    /// Dispatches the job of `step`. `pending` holds results collected
    /// during this advancement and not yet stored.
    fn start(&mut self, workflow: &Workflow, step: &StepSpec, pending: &[(String, Value)])
        -> Start;

    /// Asks for `job` to be cancelled; failures are the effect's to log.
    fn cancel(&mut self, job: JobId);
}
