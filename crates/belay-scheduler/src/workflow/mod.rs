//! DAG workflows: runs of steps, each a job, started as their dependencies
//! finish.
//!
//! The rules (eligibility, conditions, failure policies, when a run
//! succeeds) are stated in [`belay_proto::workflow`]; payload placeholders
//! are described below.
//!
//! # Driving the engine
//!
//! - [`WorkflowEngine::submit`] validates a spec, stores the run and
//!   dispatches its root steps.
//! - [`WorkflowEngine::on_job_finished`] must be called with every job that
//!   reaches a terminal state (belay-core wires it to the queue's
//!   `on_finished` hook). Jobs that are not workflow steps, or whose run is
//!   over, are ignored; reporting the same job twice is harmless.
//! - [`WorkflowEngine::recover`] runs once after opening, and may run
//!   periodically: it asks the dispatcher about every running step (to
//!   catch completions reported while the engine was down) and dispatches
//!   steps whose dispatch was deferred or lost.
//! - [`WorkflowEngine::sweep`] deletes finished runs past their retention.
//!
//! # Crash safety
//!
//! A run is stored before any of its jobs is dispatched, and every step job
//! carries the unique key `wf:<run>:<step>` (unless the step's spec brings
//! its own key). If belay stops between dispatching a step and saving that
//! fact, recovery dispatches the step again and the dispatcher returns the
//! original job, whose outcome is then picked up.
//!
//! # Payload placeholders
//!
//! A string in a step's payload may refer to the result of an ancestor
//! step as `{{steps.<name>.result}}`, optionally followed by a JSON Pointer
//! (`{{steps.fetch.result/items/0}}`) or dotted keys
//! (`{{steps.fetch.result.items.0}}`). A string that is exactly one
//! placeholder becomes the referenced value itself, of whatever type;
//! placeholders inside longer strings are replaced by text (strings as
//! they are, other values as compact JSON). Placeholders are resolved when
//! the step is dispatched; a missing value (the step did not succeed, or
//! the path leads nowhere) fails the step with an error naming the
//! placeholder, and its failure policy applies.
//!
//! # Limitations
//!
//! Retrying a step's dead job by hand after the run has moved on does not
//! reopen the step. Results are kept only for steps that some placeholder
//! reads.

mod machine;
mod record;
mod template;

pub use record::{WorkflowDetails, WorkflowFilter};

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use belay_common::id::SyncUlidGenerator;
use belay_common::sync::lock;
use belay_common::{info, warn, SharedClock, Timestamp};
use belay_json::Value;
use belay_proto::api::{Cursor, Page, SortOrder};
use belay_proto::event::EventKind;
use belay_proto::validate::{Validate, Validator};
use belay_proto::workflow::{referenced_steps, StepSpec, StepState, WorkflowSpec, WorkflowState};
use belay_proto::{EventSink, Job, JobId, Unique, UniqueScope, Workflow, WorkflowId, WorkflowRef};
use belay_store::{Kv, WriteBatch};

use crate::dispatch::Dispatcher;
use crate::error::{Error, Result};
use crate::options::WorkflowOptions;
use crate::store;

use machine::{Effects, Machine, Outcome, Start};

/// Most finished runs one [`WorkflowEngine::sweep`] call deletes, to keep
/// its write batch bounded.
pub const SWEEP_BATCH: usize = 1000;

/// The unique key of the job for step `step` of run `id`.
#[must_use]
pub fn step_key(id: WorkflowId, step: &str) -> String {
    format!("wf:{id}:{step}")
}

/// Runs DAG workflows over a [`Dispatcher`]. See the [module docs](self).
///
/// All methods take `&self`; one internal lock serialises them and is held
/// while jobs are dispatched or cancelled, so the dispatcher must not call
/// back into the engine, and `on_job_finished` must not be called while
/// holding a lock the dispatcher needs.
pub struct WorkflowEngine {
    kv: Arc<dyn Kv>,
    clock: SharedClock,
    dispatcher: Arc<dyn Dispatcher>,
    sink: Arc<dyn EventSink>,
    options: WorkflowOptions,
    ids: SyncUlidGenerator,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Runs still going, in full.
    active: BTreeMap<WorkflowId, Run>,
    /// Every stored run, for listing without reading the store.
    index: BTreeMap<WorkflowId, Summary>,
    /// Finished runs by finish time, for retention.
    finished: BTreeSet<(Timestamp, WorkflowId)>,
}

#[derive(Clone)]
struct Run {
    details: WorkflowDetails,
    /// Steps whose results some placeholder reads.
    referenced: Arc<BTreeSet<String>>,
}

impl Run {
    fn new(details: WorkflowDetails) -> Run {
        let referenced = details
            .workflow
            .spec
            .steps
            .iter()
            .flat_map(|s| referenced_steps(&s.job.payload))
            .map(str::to_owned)
            .collect();
        Run {
            details,
            referenced: Arc::new(referenced),
        }
    }
}

struct Summary {
    state: WorkflowState,
    name: String,
}

impl State {
    fn record(&mut self, details: &WorkflowDetails) {
        let wf = &details.workflow;
        self.index.insert(
            wf.id,
            Summary {
                state: wf.state,
                name: wf.spec.name.clone(),
            },
        );
    }

    /// Files a run under `active` or `finished` according to its state.
    fn install(&mut self, run: Run) {
        self.record(&run.details);
        let wf = &run.details.workflow;
        if wf.state.is_terminal() {
            let finished_at = wf.finished_at.unwrap_or(wf.created_at);
            self.finished.insert((finished_at, wf.id));
            self.active.remove(&wf.id);
        } else {
            self.active.insert(wf.id, run);
        }
    }
}

impl WorkflowEngine {
    /// Opens the engine with default options, loading every stored run.
    /// Call [`recover`](Self::recover) next.
    pub fn open(
        kv: Arc<dyn Kv>,
        clock: SharedClock,
        dispatcher: Arc<dyn Dispatcher>,
        sink: Arc<dyn EventSink>,
    ) -> Result<WorkflowEngine> {
        WorkflowEngine::open_with(kv, clock, dispatcher, sink, WorkflowOptions::default())
    }

    /// Opens the engine with `options`.
    pub fn open_with(
        kv: Arc<dyn Kv>,
        clock: SharedClock,
        dispatcher: Arc<dyn Dispatcher>,
        sink: Arc<dyn EventSink>,
        options: WorkflowOptions,
    ) -> Result<WorkflowEngine> {
        options.validate()?;
        let mut state = State::default();
        for details in store::load_all::<WorkflowDetails>(&*kv, store::workflows())? {
            state.install(Run::new(details));
        }
        Ok(WorkflowEngine {
            ids: SyncUlidGenerator::new(clock.clone()),
            kv,
            clock,
            dispatcher,
            sink,
            options,
            state: Mutex::new(state),
        })
    }

    /// The options the engine runs with.
    #[must_use]
    pub fn options(&self) -> &WorkflowOptions {
        &self.options
    }

    /// Checks `spec` as [`submit`](Self::submit) would: the DAG rules of
    /// belay-proto plus the placeholder syntax.
    pub fn validate(&self, spec: &WorkflowSpec) -> Result<()> {
        let mut v = Validator::new();
        spec.validate_into(&mut v);
        for (i, step) in spec.steps.iter().enumerate() {
            for (at, problem) in template::check(&step.job.payload) {
                let message = if at.is_root() {
                    problem.to_string()
                } else {
                    format!("{problem} (at {at})")
                };
                v.at_index("steps", i, |v| {
                    v.at_key("job", |v| v.report("payload", message))
                });
            }
        }
        v.finish().map_err(Error::Invalid)
    }

    /// Starts a run of `spec`: validates it, stores it and dispatches the
    /// root steps. The returned run shows them running (or failed, if the
    /// dispatcher refused them).
    ///
    /// If saving fails after the run was first stored, the error is
    /// returned but the run exists; [`recover`](Self::recover) picks up its
    /// already dispatched jobs.
    pub fn submit(&self, spec: WorkflowSpec) -> Result<Workflow> {
        self.validate(&spec)?;
        let now = self.clock.now();
        let id = self
            .ids
            .generate()
            .map(WorkflowId::from_ulid)
            .unwrap_or_else(|_| WorkflowId::generate());
        let mut state = lock(&self.state);
        let run = Run::new(WorkflowDetails::new(Workflow::new(id, spec, now)));
        // Stored before anything is dispatched, so that recovery finds it.
        let mut batch = WriteBatch::with_capacity(1);
        batch.put(store::workflow_key(id), store::encode(&run.details));
        self.kv.write(batch)?;
        self.sink.emit(EventKind::WorkflowStarted {
            workflow: id,
            name: run.details.workflow.spec.name.clone(),
        });
        state.install(run.clone());
        let run = self.step(&mut state, run, now, |m, fx| {
            m.advance(fx);
            true
        })?;
        Ok(run.details.workflow)
    }

    /// Advances the run of a job that reached a terminal state. Returns the
    /// updated run, or `None` if the job changed nothing (not a workflow
    /// step, not finished, its run already over, or already reported).
    pub fn on_job_finished(&self, job: &Job) -> Result<Option<Workflow>> {
        let Some(link) = &job.spec.workflow else {
            return Ok(None);
        };
        let Some(outcome) = Outcome::of_job(job) else {
            return Ok(None);
        };
        let now = self.clock.now();
        let mut state = lock(&self.state);
        let Some(run) = state.active.get(&link.id).cloned() else {
            return Ok(None);
        };
        let before = run.details.clone();
        let run = self.step(&mut state, run, now, |m, fx| {
            m.job_finished(fx, &link.step, job.id, outcome)
        })?;
        Ok((run.details != before).then_some(run.details.workflow))
    }

    /// Cancels a run: running step jobs are cancelled through the
    /// dispatcher, pending steps are marked cancelled. Cancelling a run
    /// that is already over returns it unchanged.
    pub fn cancel(&self, id: WorkflowId) -> Result<Workflow> {
        let now = self.clock.now();
        let mut state = lock(&self.state);
        let Some(run) = state.active.get(&id).cloned() else {
            return self
                .load(&state, id)?
                .map(|d| d.workflow)
                .ok_or_else(|| Error::not_found("workflow", id));
        };
        let run = self.step(&mut state, run, now, |m, fx| m.cancel(fx))?;
        Ok(run.details.workflow)
    }

    /// The run `id`.
    pub fn get(&self, id: WorkflowId) -> Result<Option<Workflow>> {
        Ok(self.details(id)?.map(|d| d.workflow))
    }

    /// The run `id` with the failure messages of its steps.
    pub fn details(&self, id: WorkflowId) -> Result<Option<WorkflowDetails>> {
        let state = lock(&self.state);
        self.load(&state, id)
    }

    /// One page of runs matching `filter`.
    pub fn list(&self, filter: &WorkflowFilter) -> Result<Page<Workflow>> {
        let limit = filter.effective_limit();
        let after = filter
            .cursor
            .as_ref()
            .and_then(Cursor::as_ulid)
            .map(WorkflowId::from_ulid);
        let state = lock(&self.state);
        let candidates: Box<dyn Iterator<Item = (&WorkflowId, &Summary)>> = match filter.order {
            SortOrder::Asc => Box::new(
                state
                    .index
                    .iter()
                    .filter(move |(id, _)| after.map_or(true, |a| **id > a)),
            ),
            SortOrder::Desc => Box::new(
                state
                    .index
                    .iter()
                    .rev()
                    .filter(move |(id, _)| after.map_or(true, |a| **id < a)),
            ),
        };
        let ids: Vec<WorkflowId> = candidates
            .filter(|(_, s)| filter.admits(s.state, &s.name))
            .take(limit as usize + 1)
            .map(|(id, _)| *id)
            .collect();
        let mut items = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(details) = self.load(&state, id)? {
                items.push(details.workflow);
            }
        }
        Ok(Page::paginate(items, limit, |w: &Workflow| {
            Cursor::from_ulid(w.id.ulid())
        }))
    }

    /// Number of stored runs.
    #[must_use]
    pub fn len(&self) -> usize {
        lock(&self.state).index.len()
    }

    /// Whether no runs are stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of runs still going.
    #[must_use]
    pub fn running(&self) -> usize {
        lock(&self.state).active.len()
    }

    /// Deletes runs that finished at least `keep_for` before `now`, oldest
    /// first and at most [`SWEEP_BATCH`] per call, with their stored
    /// results. Returns how many were deleted.
    pub fn sweep(&self, now: Timestamp, keep_for: Duration) -> Result<usize> {
        let cutoff = now - keep_for;
        let mut state = lock(&self.state);
        let victims: Vec<(Timestamp, WorkflowId)> = state
            .finished
            .iter()
            .take_while(|(finished_at, _)| *finished_at <= cutoff)
            .take(SWEEP_BATCH)
            .copied()
            .collect();
        if victims.is_empty() {
            return Ok(0);
        }
        let mut batch = WriteBatch::with_capacity(victims.len() * 2);
        for (_, id) in &victims {
            batch.delete(store::workflow_key(*id));
            batch.delete_prefix(store::results_prefix(*id));
        }
        self.kv.write(batch)?;
        for entry in &victims {
            state.finished.remove(entry);
            state.index.remove(&entry.1);
        }
        info!("deleted finished workflow runs"; count = victims.len(), before = cutoff);
        Ok(victims.len())
    }

    /// Reconciles every running run with the dispatcher: steps whose jobs
    /// finished while the engine was not told (or was down) are finished,
    /// steps whose jobs vanished fail, and eligible steps without a job are
    /// dispatched. Returns how many runs changed.
    pub fn recover(&self) -> Result<usize> {
        let now = self.clock.now();
        let mut state = lock(&self.state);
        let ids: Vec<WorkflowId> = state.active.keys().copied().collect();
        let mut changed = 0;
        for id in ids {
            let Some(run) = state.active.get(&id).cloned() else {
                continue;
            };
            let before = run.details.clone();
            let dispatcher = Arc::clone(&self.dispatcher);
            let run = self.step(&mut state, run, now, |m, fx| {
                for (step, job) in running_steps(&before.workflow) {
                    if let Some(outcome) = probe(&*dispatcher, job) {
                        m.job_finished(fx, &step, job, outcome);
                    }
                }
                m.advance(fx);
                true
            })?;
            if run.details != before {
                changed += 1;
            }
        }
        if changed > 0 {
            info!("recovered workflow runs"; changed);
        }
        Ok(changed)
    }

    /// Runs `f` on a machine over `run`, then saves the run with any new
    /// results in one batch, files it and publishes the events. Nothing is
    /// saved or published if `f` reports no change.
    fn step(
        &self,
        state: &mut State,
        mut run: Run,
        now: Timestamp,
        f: impl FnOnce(&mut Machine<'_>, &mut dyn Effects) -> bool,
    ) -> Result<Run> {
        let id = run.details.workflow.id;
        let referenced = Arc::clone(&run.referenced);
        let (events, results) = {
            let mut fx = Live {
                engine: self,
                id,
                kv: &*self.kv,
            };
            let mut machine = Machine::new(&mut run.details, &referenced, now);
            if !f(&mut machine, &mut fx) {
                return Ok(run);
            }
            (machine.events, machine.results)
        };
        if events.is_empty() {
            return Ok(run);
        }
        let mut batch = WriteBatch::with_capacity(1 + results.len());
        batch.put(store::workflow_key(id), store::encode(&run.details));
        for (step, result) in &results {
            batch.put(store::result_key(id, step), store::encode(result));
        }
        self.kv.write(batch)?;
        state.install(run.clone());
        for event in events {
            self.sink.emit(event);
        }
        Ok(run)
    }

    fn load(&self, state: &State, id: WorkflowId) -> Result<Option<WorkflowDetails>> {
        if let Some(run) = state.active.get(&id) {
            return Ok(Some(run.details.clone()));
        }
        if !state.index.contains_key(&id) {
            return Ok(None);
        }
        store::load(&*self.kv, &store::workflow_key(id))
    }
}

/// Steps with a dispatched job still believed to be running.
fn running_steps(wf: &Workflow) -> Vec<(String, JobId)> {
    wf.steps
        .iter()
        .filter_map(|(name, s)| match (s.state, s.job) {
            (StepState::Running, Some(job)) => Some((name.clone(), job)),
            _ => None,
        })
        .collect()
}

/// What the dispatcher knows about a step's job: its outcome if it has
/// finished (or a failure if it no longer exists), `None` while it runs.
fn probe(dispatcher: &dyn Dispatcher, job: JobId) -> Option<Outcome> {
    if let Some(record) = dispatcher.job(&job) {
        return Outcome::of_job(&record);
    }
    match dispatcher.job_state(&job) {
        Some(state) => Outcome::of_state(job, state),
        None => Some(Outcome::Failed(format!("job {job} no longer exists"))),
    }
}

/// The machine's effects on the real dispatcher and store.
struct Live<'e> {
    engine: &'e WorkflowEngine,
    id: WorkflowId,
    kv: &'e dyn Kv,
}

impl Live<'_> {
    /// The stored or just-collected result of `step`, or why there is none.
    fn result_of(
        &self,
        wf: &Workflow,
        step: &str,
        pending: &[(String, Value)],
    ) -> Result<Value, String> {
        match wf.step_state(step) {
            Some(StepState::Succeeded) => {}
            Some(other) => return Err(format!("step {step:?} is {other}, so it has no result")),
            None => return Err(format!("there is no step {step:?}")),
        }
        if let Some((_, value)) = pending.iter().find(|(name, _)| name == step) {
            return Ok(value.clone());
        }
        match store::load::<Value>(self.kv, &store::result_key(self.id, step)) {
            Ok(Some(value)) => Ok(value),
            Ok(None) => Err(format!(
                "the result of step {step:?} is not available (its completion was recovered without the job record)"
            )),
            Err(e) => Err(format!("reading the result of step {step:?} failed: {e}")),
        }
    }
}

impl Effects for Live<'_> {
    fn start(&mut self, wf: &Workflow, step: &StepSpec, pending: &[(String, Value)]) -> Start {
        let mut job = step.job.clone();
        if template::has_placeholders(&job.payload) {
            match template::render(&job.payload, &mut |name| self.result_of(wf, name, pending)) {
                Ok(payload) => job.payload = payload,
                Err(reason) => return Start::Failed(reason),
            }
        }
        job.workflow = Some(WorkflowRef {
            id: self.id,
            step: step.name.clone(),
        });
        if job.unique.is_none() {
            job.unique = Some(Unique::new(
                step_key(self.id, &step.name),
                UniqueScope::For(self.engine.options.get_dedupe_window()),
            ));
        }
        let dispatcher = &*self.engine.dispatcher;
        match dispatcher.dispatch(&job) {
            Ok(outcome) if outcome.duplicate => {
                // An earlier attempt at this step already has a job; it may
                // even have finished while nobody was listening.
                match probe(dispatcher, outcome.id) {
                    Some(result) => Start::Finished(outcome.id, result),
                    None => Start::Running(outcome.id),
                }
            }
            Ok(outcome) => Start::Running(outcome.id),
            Err(e) if e.is_transient() => {
                warn!("dispatcher unavailable, the step waits for recovery";
                    workflow = %self.id, step = %step.name, error = %e);
                Start::Deferred(e.message().to_owned())
            }
            Err(e) => Start::Failed(format!("dispatch rejected: {}", e.message())),
        }
    }

    fn cancel(&mut self, job: JobId) {
        if let Err(e) = self.engine.dispatcher.cancel(&job) {
            warn!("cancelling a step's job failed"; workflow = %self.id, job = %job, error = %e);
        }
    }
}

impl fmt::Debug for WorkflowEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = lock(&self.state);
        f.debug_struct("WorkflowEngine")
            .field("runs", &state.index.len())
            .field("running", &state.active.len())
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}
