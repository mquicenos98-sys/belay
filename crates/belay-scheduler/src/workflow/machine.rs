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

/// One advancement of one run.
pub(crate) struct Machine<'a> {
    run: &'a mut WorkflowDetails,
    /// Steps whose results some placeholder reads.
    referenced: &'a BTreeSet<String>,
    now: Timestamp,
    /// Events to publish once the run is saved.
    pub(crate) events: Vec<EventKind>,
    /// Results to save alongside the run.
    pub(crate) results: Vec<(String, Value)>,
    /// Steps whose dispatch was deferred during this advancement.
    deferred: BTreeSet<String>,
}

impl<'a> Machine<'a> {
    pub(crate) fn new(
        run: &'a mut WorkflowDetails,
        referenced: &'a BTreeSet<String>,
        now: Timestamp,
    ) -> Machine<'a> {
        Machine {
            run,
            referenced,
            now,
            events: Vec::new(),
            results: Vec::new(),
            deferred: BTreeSet::new(),
        }
    }

    fn wf(&self) -> &Workflow {
        &self.run.workflow
    }

    /// Records that `job`, dispatched for `step`, ended with `outcome`, and
    /// advances. Returns `false` (changing nothing) when the report does not
    /// apply: the run is over, the step already finished, or the job is not
    /// the step's.
    pub(crate) fn job_finished(
        &mut self,
        fx: &mut dyn Effects,
        step: &str,
        job: JobId,
        outcome: Outcome,
    ) -> bool {
        if self.wf().state.is_terminal() {
            return false;
        }
        let Some(status) = self.wf().steps.get(step) else {
            return false;
        };
        match (status.state, status.job) {
            (StepState::Running, Some(current)) if current == job => {}
            // The dispatch happened but its bookkeeping was lost to a crash;
            // the job's workflow reference proves it belongs here.
            (StepState::Pending, None) if self.is_eligible(step) => self.mark_running(step, job),
            _ => return false,
        }
        self.finish_step(fx, step, outcome);
        self.advance(fx);
        true
    }

    /// Starts or skips every eligible step, repeatedly, then settles the
    /// run's own state.
    pub(crate) fn advance(&mut self, fx: &mut dyn Effects) {
        loop {
            if self.wf().state.is_terminal() {
                break;
            }
            let eligible: Vec<String> = self
                .wf()
                .eligible_steps()
                .into_iter()
                .map(|s| s.name.clone())
                .filter(|name| !self.deferred.contains(name))
                .collect();
            if eligible.is_empty() {
                break;
            }
            for name in eligible {
                if self.wf().state.is_terminal() {
                    break;
                }
                // An earlier step in this pass may have skipped it.
                if self.wf().step_state(&name) != Some(StepState::Pending) {
                    continue;
                }
                let Some(step) = self.wf().spec.get_step(&name).cloned() else {
                    continue;
                };
                let parents = self.wf().parent_states(&step);
                if !step.condition().is_satisfied(&parents) {
                    self.set_state(&name, StepState::Skipped);
                    continue;
                }
                match fx.start(&self.run.workflow, &step, &self.results) {
                    Start::Running(job) => self.mark_running(&name, job),
                    Start::Finished(job, outcome) => {
                        self.mark_running(&name, job);
                        self.finish_step(fx, &name, outcome);
                    }
                    Start::Failed(reason) => self.finish_step(fx, &name, Outcome::Failed(reason)),
                    Start::Deferred(_) => {
                        self.deferred.insert(name);
                    }
                }
            }
        }
        self.settle();
    }

    /// Cancels the run: pending steps are cancelled, running ones too
    /// (and their jobs). Returns `false` if the run was already over.
    pub(crate) fn cancel(&mut self, fx: &mut dyn Effects) -> bool {
        if self.wf().state.is_terminal() {
            return false;
        }
        self.stop_everything(fx);
        self.end(WorkflowState::Cancelled);
        true
    }

    fn is_eligible(&self, step: &str) -> bool {
        self.wf().eligible_steps().iter().any(|s| s.name == step)
    }

    fn mark_running(&mut self, name: &str, job: JobId) {
        let now = self.now;
        if let Some(status) = self.run.workflow.steps.get_mut(name) {
            status.state = StepState::Running;
            status.job = Some(job);
            status.started_at = Some(now);
        }
        self.events.push(EventKind::WorkflowStep {
            workflow: self.run.workflow.id,
            step: name.to_owned(),
            state: StepState::Running,
            job: Some(job),
        });
    }

    /// Moves a step to a finished state and records the event.
    fn set_state(&mut self, name: &str, state: StepState) {
        let now = self.now;
        let Some(status) = self.run.workflow.steps.get_mut(name) else {
            return;
        };
        status.state = state;
        if state.is_finished() {
            status.finished_at = Some(now);
        }
        let job = status.job;
        self.events.push(EventKind::WorkflowStep {
            workflow: self.run.workflow.id,
            step: name.to_owned(),
            state,
            job,
        });
    }

    fn finish_step(&mut self, fx: &mut dyn Effects, name: &str, outcome: Outcome) {
        match outcome {
            Outcome::Succeeded(result) => {
                self.set_state(name, StepState::Succeeded);
                if let Some(result) = result {
                    if self.referenced.contains(name) {
                        self.results.push((name.to_owned(), result));
                    }
                }
            }
            Outcome::Failed(reason) => {
                self.run.errors.insert(name.to_owned(), reason);
                self.set_state(name, StepState::Failed);
                let policy = self
                    .wf()
                    .spec
                    .get_step(name)
                    .map_or(StepFailurePolicy::default(), |s| s.on_failure);
                match policy {
                    StepFailurePolicy::FailWorkflow => {
                        self.stop_everything(fx);
                        self.end(WorkflowState::Failed);
                    }
                    StepFailurePolicy::SkipDependents => {
                        let downstream: Vec<String> = self
                            .wf()
                            .spec
                            .dependents(name)
                            .into_iter()
                            .map(str::to_owned)
                            .collect();
                        for step in downstream {
                            if self.wf().step_state(&step) == Some(StepState::Pending) {
                                self.set_state(&step, StepState::Skipped);
                            }
                        }
                    }
                    StepFailurePolicy::Continue => {}
                }
            }
        }
    }

    /// Cancels every step still pending or running.
    fn stop_everything(&mut self, fx: &mut dyn Effects) {
        let open: Vec<(String, StepState, Option<JobId>)> = self
            .wf()
            .spec
            .steps
            .iter()
            .filter_map(|s| {
                let status = self.wf().steps.get(&s.name)?;
                (!status.state.is_finished()).then(|| (s.name.clone(), status.state, status.job))
            })
            .collect();
        for (name, state, job) in open {
            if let (StepState::Running, Some(job)) = (state, job) {
                fx.cancel(job);
            }
            self.set_state(&name, StepState::Cancelled);
        }
    }

    /// Finishes the run once every step has finished.
    fn settle(&mut self) {
        if !self.wf().state.is_terminal() && self.wf().all_steps_finished() {
            self.end(WorkflowState::Succeeded);
        }
    }

    fn end(&mut self, state: WorkflowState) {
        let wf = &mut self.run.workflow;
        wf.state = state;
        wf.finished_at = Some(self.now);
        self.events.push(EventKind::WorkflowFinished {
            workflow: wf.id,
            name: wf.spec.name.clone(),
            state,
        });
    }
}
