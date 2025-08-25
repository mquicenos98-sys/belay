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
