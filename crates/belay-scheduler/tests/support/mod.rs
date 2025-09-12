//! Helpers shared by the integration tests: a scheduler or workflow engine
//! wired to a manual clock, an in-memory dispatcher and a collecting event
//! sink, plus a store wrapper that can refuse writes to simulate a crash
//! before bookkeeping is saved.
// Each test binary uses a different subset of these helpers.
#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use belay_common::log::{set_max_level, LevelFilter};
use belay_common::{Clock, ManualClock, Timestamp};
use belay_json::Value;
use belay_proto::source::CollectingSink;
use belay_proto::workflow::{StepSpec, WorkflowSpec};
use belay_proto::{Job, JobSpec, ScheduleId, ScheduleSpec, Workflow, WorkflowId};
use belay_scheduler::{
    MemoryDispatcher, Scheduler, SchedulerOptions, TickReport, WorkflowEngine, WorkflowOptions,
};
use belay_store::{CheckpointInfo, KeyRange, Kv, KvStats, MemKv, StoreError, WriteBatch};

/// Keeps expected warnings (rejected dispatches, failing stores) out of
/// the test output.
pub fn quiet() {
    set_max_level(LevelFilter::Off);
}

pub fn ts(s: &str) -> Timestamp {
    s.parse().expect("test timestamps are valid RFC 3339")
}

pub fn id(s: &str) -> ScheduleId {
    s.parse().expect("test schedule ids are valid")
}

/// A schedule firing `work` jobs on queue `jobs`.
pub fn spec(name: &str, cron: &str) -> ScheduleSpec {
    ScheduleSpec::new(id(name), cron, JobSpec::new("jobs", "work"))
}

/// Fire times of a report, as RFC 3339 text.
pub fn fired_times(report: &TickReport) -> Vec<String> {
    report
        .fired
        .iter()
        .map(|f| f.fire_time.to_string())
        .collect()
}

/// A store that can be told to refuse writes, as if the process died just
/// before saving.
#[derive(Debug)]
pub struct FlakyKv {
    inner: Arc<dyn Kv>,
    refuse: AtomicBool,
    /// Writes still allowed before refusing; negative for no limit.
    budget: AtomicI64,
}

impl FlakyKv {
    pub fn new(inner: Arc<dyn Kv>) -> Arc<FlakyKv> {
        Arc::new(FlakyKv {
            inner,
            refuse: AtomicBool::new(false),
            budget: AtomicI64::new(-1),
        })
    }

    pub fn refuse_writes(&self, refuse: bool) {
        self.refuse.store(refuse, Ordering::SeqCst);
        self.budget.store(-1, Ordering::SeqCst);
    }

    /// Allows `n` more writes, then refuses the rest.
    pub fn fail_after(&self, n: i64) {
        self.budget.store(n, Ordering::SeqCst);
    }

    fn admit(&self) -> bool {
        if self.refuse.load(Ordering::SeqCst) {
            return false;
        }
        let left = self.budget.load(Ordering::SeqCst);
        if left < 0 {
            return true;
        }
        if left == 0 {
            self.refuse.store(true, Ordering::SeqCst);
            return false;
        }
        self.budget.store(left - 1, Ordering::SeqCst);
        true
    }
}

impl Kv for FlakyKv {
    fn get(&self, key: &[u8]) -> belay_store::Result<Option<Vec<u8>>> {
        self.inner.get(key)
    }

    fn scan(&self, range: KeyRange) -> belay_store::Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.inner.scan(range)
    }

    fn write(&self, batch: WriteBatch) -> belay_store::Result<u64> {
        if !self.admit() {
            return Err(StoreError::Failed {
                reason: "simulated crash before the write".into(),
            });
        }
        self.inner.write(batch)
    }

    fn flush(&self) -> belay_store::Result<()> {
        self.inner.flush()
    }

    fn stats(&self) -> KvStats {
        self.inner.stats()
    }

    fn checkpoint(&self) -> belay_store::Result<CheckpointInfo> {
        self.inner.checkpoint()
    }
}

/// A scheduler over a manual clock and an in-memory dispatcher.
pub struct Harness {
    pub clock: ManualClock,
    pub queue: Arc<MemoryDispatcher>,
    pub sink: Arc<CollectingSink>,
    pub kv: Arc<dyn Kv>,
    pub scheduler: Scheduler,
}

impl Harness {
    /// Starts at `start` on an empty in-memory store.
    pub fn new(start: &str) -> Harness {
        Harness::with(start, Arc::new(MemKv::new()), SchedulerOptions::new())
    }

    pub fn with(start: &str, kv: Arc<dyn Kv>, options: SchedulerOptions) -> Harness {
        quiet();
        let clock = ManualClock::new(ts(start));
        let queue = Arc::new(MemoryDispatcher::new(clock.shared()));
        let sink = Arc::new(CollectingSink::new());
        let scheduler = Scheduler::open_with(
            Arc::clone(&kv),
            clock.shared(),
            queue.clone(),
            sink.clone(),
            options,
        )
        .expect("opening the scheduler");
        Harness {
            clock,
            queue,
            sink,
            kv,
            scheduler,
        }
    }

    /// Simulates a restart: a new scheduler over the same store, queue and
    /// clock.
    pub fn restart(self) -> Harness {
        self.restart_with(SchedulerOptions::new())
    }

    pub fn restart_with(self, options: SchedulerOptions) -> Harness {
        let Harness {
            clock,
            queue,
            sink,
            kv,
            scheduler,
        } = self;
        drop(scheduler);
        let scheduler = Scheduler::open_with(
            Arc::clone(&kv),
            clock.shared(),
            queue.clone(),
            sink.clone(),
            options,
        )
        .expect("reopening the scheduler");
        Harness {
            clock,
            queue,
            sink,
            kv,
            scheduler,
        }
    }

    pub fn now(&self) -> Timestamp {
        self.clock.now()
    }

    /// Moves the clock to `t` and ticks.
    pub fn tick_at(&self, t: &str) -> TickReport {
        self.clock.set(ts(t));
        self.scheduler.tick(self.clock.now())
    }

    /// Advances the clock by `d` and ticks.
    pub fn tick_after(&self, d: Duration) -> TickReport {
        let now = self.clock.advance(d);
        self.scheduler.tick(now)
    }

    /// Ticks at every wakeup up to `until`, collecting the reports.
    pub fn run_until(&self, until: &str) -> Vec<TickReport> {
        let until = ts(until);
        let mut reports = Vec::new();
        while let Some(wake) = self.scheduler.next_wakeup() {
            if wake > until {
                break;
            }
            self.clock.set(wake.max(self.clock.now()));
            reports.push(self.scheduler.tick(self.clock.now()));
        }
        self.clock.set(until);
        reports
    }

    /// Event type names emitted since the last call.
    pub fn events(&self) -> Vec<&'static str> {
        let names = self.sink.type_names();
        self.sink.take();
        names
    }
}

/// A workflow engine over a manual clock and an in-memory dispatcher.
pub struct Flow {
    pub clock: ManualClock,
    pub queue: Arc<MemoryDispatcher>,
    pub sink: Arc<CollectingSink>,
    pub kv: Arc<dyn Kv>,
    pub engine: WorkflowEngine,
}

impl Flow {
    pub fn new() -> Flow {
        Flow::with(Arc::new(MemKv::new()))
    }

    pub fn with(kv: Arc<dyn Kv>) -> Flow {
        quiet();
        let clock = ManualClock::new(ts("2025-05-05T10:00:00Z"));
        let queue = Arc::new(MemoryDispatcher::new(clock.shared()));
        let sink = Arc::new(CollectingSink::new());
        let engine = WorkflowEngine::open_with(
            Arc::clone(&kv),
            clock.shared(),
            queue.clone(),
            sink.clone(),
            WorkflowOptions::new(),
        )
        .expect("opening the workflow engine");
        Flow {
            clock,
            queue,
            sink,
            kv,
            engine,
        }
    }

    /// A new engine over the same store, queue and clock.
    pub fn restart(self) -> Flow {
        let Flow {
            clock,
            queue,
            sink,
            kv,
            engine,
        } = self;
        drop(engine);
        let engine =
            WorkflowEngine::open(Arc::clone(&kv), clock.shared(), queue.clone(), sink.clone())
                .expect("reopening the workflow engine");
        Flow {
            clock,
            queue,
            sink,
            kv,
            engine,
        }
    }

    pub fn submit(&self, spec: WorkflowSpec) -> Workflow {
        self.engine.submit(spec).expect("submitting the workflow")
    }

    /// The job dispatched for `step` of `run`.
    pub fn job(&self, run: WorkflowId, step: &str) -> Job {
        self.queue
            .find_step(run, step)
            .unwrap_or_else(|| panic!("no job was dispatched for step {step}"))
    }

    /// Whether a job was dispatched for `step`.
    pub fn dispatched(&self, run: WorkflowId, step: &str) -> bool {
        self.queue.find_step(run, step).is_some()
    }

    /// Completes `step`'s job with `result` and reports it to the engine.
    pub fn succeed(&self, run: WorkflowId, step: &str, result: Value) -> Workflow {
        self.clock.advance(Duration::from_secs(1));
        let job = self
            .queue
            .succeed(&self.job(run, step).id, result)
            .expect("the step's job was still pending");
        self.engine
            .on_job_finished(&job)
            .expect("advancing the workflow")
            .expect("the completion changed the workflow")
    }

    /// Kills `step`'s job with `message` and reports it to the engine.
    pub fn fail(&self, run: WorkflowId, step: &str, message: &str) -> Workflow {
        self.clock.advance(Duration::from_secs(1));
        let job = self
            .queue
            .fail(&self.job(run, step).id, message)
            .expect("the step's job was still pending");
        self.engine
            .on_job_finished(&job)
            .expect("advancing the workflow")
            .expect("the failure changed the workflow")
    }

    pub fn get(&self, run: WorkflowId) -> Workflow {
        self.engine
            .get(run)
            .expect("reading the workflow")
            .expect("the workflow exists")
    }

    pub fn events(&self) -> Vec<&'static str> {
        let names = self.sink.type_names();
        self.sink.take();
        names
    }
}

/// A step running `kind` on queue `etl`, after `deps`.
pub fn step(name: &str, deps: &[&str]) -> StepSpec {
    deps.iter()
        .fold(StepSpec::new(name, JobSpec::new("etl", name)), |s, d| {
            s.after(*d)
        })
}

pub fn workflow(name: &str, steps: Vec<StepSpec>) -> WorkflowSpec {
    steps
        .into_iter()
        .fold(WorkflowSpec::new(name), WorkflowSpec::step)
}
