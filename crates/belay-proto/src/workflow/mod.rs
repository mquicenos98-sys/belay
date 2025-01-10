//! Workflows: a DAG of steps, each running one job once its dependencies
//! have finished.
//!
//! # Semantics
//!
//! These rules are implemented by the scheduler's workflow engine; they are
//! stated here because they define what the types mean.
//!
//! - A step becomes eligible once every step it depends on has finished
//!   (succeeded, failed, skipped or cancelled). Its [`Condition`] then
//!   decides between running it and marking it `skipped`.
//! - When a step fails (its job ends `dead` or `cancelled`), its
//!   [`StepFailurePolicy`] applies: `fail_workflow` cancels everything still
//!   pending or running and fails the workflow; `skip_dependents` skips
//!   every step downstream of it, whatever their conditions; `continue`
//!   lets downstream conditions decide.
//! - The workflow succeeds when every step has finished and no step failed
//!   under `fail_workflow`.
//! - Step payloads may contain `{{steps.<name>.result...}}` placeholders,
//!   resolved from earlier results when the step is dispatched. Validation
//!   checks that `<name>` is an ancestor of the step.

mod dag;

pub use dag::referenced_steps;

use std::collections::BTreeMap;

use belay_common::Timestamp;
use belay_json::{FromJson, FromJsonError, ObjectBuilder, ObjectReader, ToJson, Value};

use crate::ids::{JobId, WorkflowId};
use crate::job::JobSpec;
use crate::wire::{text_enum, BuilderExt, ReaderExt};

text_enum! {
    /// What a step's failure does to the rest of the workflow.
    #[derive(Default)]
    StepFailurePolicy, "step failure policy" {
        /// Cancel everything else and fail the workflow (the default).
        #[default]
        FailWorkflow = "fail_workflow",
        /// Keep going; downstream conditions decide what runs.
        Continue = "continue",
        /// Skip everything downstream of the failed step.
        SkipDependents = "skip_dependents",
    }
}

text_enum! {
    /// When an eligible step runs, judged by how its dependencies ended.
    /// Steps without dependencies always run.
    #[derive(Default)]
    Condition, "step condition" {
        /// Every dependency succeeded (the default).
        #[default]
        AllSucceeded = "all_succeeded",
        /// At least one dependency succeeded.
        AnySucceeded = "any_succeeded",
        /// At least one dependency failed or was cancelled: a cleanup or
        /// compensation step.
        AnyFailed = "any_failed",
        /// Whatever happened.
        Always = "always",
    }
}

impl Condition {
    /// Whether a step with this condition runs, given the final states of
    /// its dependencies (all of which must have finished).
    #[must_use]
    pub fn is_satisfied(self, parents: &[StepState]) -> bool {
        if parents.is_empty() {
            return true;
        }
        let failed = |s: &StepState| matches!(s, StepState::Failed | StepState::Cancelled);
        match self {
            Condition::AllSucceeded => parents.iter().all(|s| *s == StepState::Succeeded),
            Condition::AnySucceeded => parents.contains(&StepState::Succeeded),
            Condition::AnyFailed => parents.iter().any(failed),
            Condition::Always => true,
        }
    }
}

text_enum! {
    /// Where a workflow run is.
    WorkflowState, "workflow state" {
        /// Steps are still pending or running.
        Running = "running",
        /// Every step finished and none failed the workflow.
        Succeeded = "succeeded",
        /// A step failed under `fail_workflow`.
        Failed = "failed",
        /// Cancelled on request.
        Cancelled = "cancelled",
    }
}

impl WorkflowState {
    /// Whether the run is over.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, WorkflowState::Running)
    }
}

text_enum! {
    /// Where one step of a run is.
    StepState, "step state" {
        /// Waiting for dependencies.
        Pending = "pending",
        /// Its job has been dispatched.
        Running = "running",
        /// Its job succeeded.
        Succeeded = "succeeded",
        /// Its job ended dead or cancelled.
        Failed = "failed",
        /// Not run: its condition was false or an upstream failure skipped it.
        Skipped = "skipped",
        /// Cancelled with the workflow.
        Cancelled = "cancelled",
    }
}

impl StepState {
    /// Whether the step will not change any more.
    #[must_use]
    pub const fn is_finished(self) -> bool {
        !matches!(self, StepState::Pending | StepState::Running)
    }
}

/// One step: a job plus its place in the graph.
#[derive(Clone, Debug, PartialEq)]
pub struct StepSpec {
    /// Unique within the workflow: `[a-z0-9][a-z0-9_-]{0,63}`.
    pub name: String,
    /// The job to run. Its `workflow` field is set by the engine.
    pub job: JobSpec,
    /// Steps that must finish first.
    pub depends_on: Vec<String>,
    /// What this step's failure does.
    pub on_failure: StepFailurePolicy,
    /// When to run once eligible; `None` means `all_succeeded`.
    pub when: Option<Condition>,
}

impl StepSpec {
    /// A root step with default policies.
    #[must_use]
    pub fn new(name: impl Into<String>, job: JobSpec) -> StepSpec {
        StepSpec {
            name: name.into(),
            job,
            depends_on: Vec::new(),
            on_failure: StepFailurePolicy::default(),
            when: None,
        }
    }

    /// Adds a dependency.
    #[must_use]
    pub fn after(mut self, step: impl Into<String>) -> StepSpec {
        self.depends_on.push(step.into());
        self
    }

    /// Sets the failure policy.
    #[must_use]
    pub fn on_failure(mut self, policy: StepFailurePolicy) -> StepSpec {
        self.on_failure = policy;
        self
    }

    /// Sets the run condition.
    #[must_use]
    pub fn when(mut self, condition: Condition) -> StepSpec {
        self.when = Some(condition);
        self
    }

    /// The effective run condition.
    #[must_use]
    pub fn condition(&self) -> Condition {
        self.when.unwrap_or_default()
    }
}

impl ToJson for StepSpec {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("name", &self.name)
            .field("job", &self.job)
            .field_if(!self.depends_on.is_empty(), "depends_on", &self.depends_on)
            .field("on_failure", self.on_failure)
            .opt_field("when", &self.when)
            .build()
    }
}

impl FromJson for StepSpec {
    fn from_json(value: &Value) -> Result<StepSpec, FromJsonError> {
        let r = ObjectReader::new(value, "StepSpec")?;
        let step = StepSpec {
            name: r.req("name")?,
            job: r.req("job")?,
            depends_on: r.or_default("depends_on")?,
            on_failure: r.or_default("on_failure")?,
            when: r.opt("when")?,
        };
        r.deny_unknown_fields()?;
        Ok(step)
    }
}

/// A workflow definition as submitted.
///
/// ```
/// use belay_proto::workflow::{Condition, StepSpec, WorkflowSpec};
/// use belay_proto::{JobSpec, Validate};
///
/// let wf = WorkflowSpec::new("nightly-etl")
///     .step(StepSpec::new("extract", JobSpec::new("etl", "extract")))
///     .step(StepSpec::new("load", JobSpec::new("etl", "load")).after("extract"))
///     .step(StepSpec::new("alert", JobSpec::new("ops", "page")).after("load").when(Condition::AnyFailed));
/// assert!(wf.validate().is_ok());
/// assert_eq!(wf.topological_order().unwrap(), ["extract", "load", "alert"]);
///
/// let cyclic = WorkflowSpec::new("bad")
///     .step(StepSpec::new("a", JobSpec::new("q", "k")).after("b"))
///     .step(StepSpec::new("b", JobSpec::new("q", "k")).after("a"));
/// assert_eq!(
///     cyclic.validate().unwrap_err().to_string(),
///     "$.steps: dependency cycle: a -> b -> a (each step depends on the next)"
/// );
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowSpec {
    /// A label for humans and filtering.
    pub name: String,
    /// The steps, in declaration order (which breaks ties in scheduling).
    pub steps: Vec<StepSpec>,
    /// String annotations, same rules as job `meta`.
    pub meta: BTreeMap<String, String>,
}

impl WorkflowSpec {
    /// An empty workflow; add steps with [`step`](Self::step).
    #[must_use]
    pub fn new(name: impl Into<String>) -> WorkflowSpec {
        WorkflowSpec {
            name: name.into(),
            steps: Vec::new(),
            meta: BTreeMap::new(),
        }
    }

    /// Appends a step.
    #[must_use]
    pub fn step(mut self, step: StepSpec) -> WorkflowSpec {
        self.steps.push(step);
        self
    }

    /// Looks up a step by name.
    #[must_use]
    pub fn get_step(&self, name: &str) -> Option<&StepSpec> {
        self.steps.iter().find(|s| s.name == name)
    }
}

impl ToJson for WorkflowSpec {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("name", &self.name)
            .field("steps", &self.steps)
            .field_if(!self.meta.is_empty(), "meta", &self.meta)
            .build()
    }
}

impl FromJson for WorkflowSpec {
    fn from_json(value: &Value) -> Result<WorkflowSpec, FromJsonError> {
        let r = ObjectReader::new(value, "WorkflowSpec")?;
        let spec = WorkflowSpec {
            name: r.req("name")?,
            steps: r.req("steps")?,
            meta: r.or_default("meta")?,
        };
        r.deny_unknown_fields()?;
        Ok(spec)
    }
}

/// The progress of one step in a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepStatus {
    /// Where the step is.
    pub state: StepState,
    /// The job dispatched for it, once dispatched.
    pub job: Option<JobId>,
    /// When its job was dispatched.
    pub started_at: Option<Timestamp>,
    /// When it finished.
    pub finished_at: Option<Timestamp>,
}
