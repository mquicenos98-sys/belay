//! Durability: reopening both engines on a log-structured store over an
//! in-memory file system (including after a simulated power loss), and
//! workflow recovery after completions or bookkeeping were lost.

mod support;

use std::sync::Arc;
use std::time::Duration;

use belay_common::{Clock, ManualClock};
use belay_json::{json, Value};
use belay_proto::source::CollectingSink;
use belay_proto::workflow::{StepState, WorkflowState};
use belay_proto::{Job, JobId, JobSpec, JobState, MisfirePolicy};
use belay_scheduler::{
    DispatchError, DispatchOutcome, Dispatcher, MemoryDispatcher, Scheduler, WorkflowEngine,
};
use belay_store::vfs::{Crash, MemFs};
use belay_store::{Kv, LogKv, MemKv, Options, SyncPolicy};

use support::{id, quiet, spec, step, ts, workflow, FlakyKv, Flow};

fn open_log(fs: &MemFs) -> Arc<dyn Kv> {
    quiet();
    Arc::new(
        LogKv::open(
            "store",
            Options::new()
                .fs(fs.clone())
                .sync(SyncPolicy::Always)
                .manual_checkpoints(),
        )
        .expect("opening the store"),
    )
}

struct Parts {
    clock: ManualClock,
    queue: Arc<MemoryDispatcher>,
    sink: Arc<CollectingSink>,
}

impl Parts {
    fn new(start: &str) -> Parts {
        let clock = ManualClock::new(ts(start));
        Parts {
            queue: Arc::new(MemoryDispatcher::new(clock.shared())),
            sink: Arc::new(CollectingSink::new()),
            clock,
        }
    }

    fn scheduler(&self, kv: Arc<dyn Kv>) -> Scheduler {
        Scheduler::open(
            kv,
            self.clock.shared(),
            self.queue.clone(),
            self.sink.clone(),
        )
        .expect("opening the scheduler")
    }

    fn engine(&self, kv: Arc<dyn Kv>) -> WorkflowEngine {
        WorkflowEngine::open(
            kv,
            self.clock.shared(),
            self.queue.clone(),
            self.sink.clone(),
        )
        .expect("opening the workflow engine")
    }
}

#[test]
fn schedules_and_calendars_survive_reopening() {
    let fs = MemFs::new();
    let parts = Parts::new("2025-06-02T10:07:00Z");
    {
        let scheduler = parts.scheduler(open_log(&fs));
        scheduler
            .put_calendar(
                belay_json::from_str(r#"{"name": "weekdays", "exclude_weekdays": ["sat", "sun"]}"#)
                    .unwrap(),
            )
            .unwrap();
        scheduler
            .create(spec("sync", "*/15 * * * *").calendar("weekdays"))
            .unwrap();
        scheduler
            .create(spec("nightly", "0 2 * * *").misfire(MisfirePolicy::Skip))
            .unwrap();
        scheduler.pause(&id("nightly")).unwrap();
        parts.clock.set(ts("2025-06-02T10:15:00Z"));
        assert_eq!(scheduler.tick(parts.clock.now()).fired.len(), 1);
    }
    let before = parts.queue.job_count();
    let scheduler = parts.scheduler(open_log(&fs));
    let sync = scheduler.get(&id("sync")).unwrap();
    assert_eq!(sync.fire_count, 1);
    assert_eq!(sync.next_fire_at, Some(ts("2025-06-02T10:30:00Z")));
    assert!(scheduler.get(&id("nightly")).unwrap().spec.paused);
    assert!(scheduler.get_calendar("weekdays").is_some());
    // It carries on where it stopped.
    parts.clock.set(ts("2025-06-02T10:30:00Z"));
    assert_eq!(scheduler.tick(parts.clock.now()).fired.len(), 1);
    assert_eq!(parts.queue.job_count(), before + 1);
}

#[test]
fn a_power_loss_keeps_every_saved_fire() {
    let fs = MemFs::new();
    let parts = Parts::new("2025-06-02T10:07:00Z");
    let scheduler = parts.scheduler(open_log(&fs));
    scheduler.create(spec("sync", "*/15 * * * *")).unwrap();
    for t in ["2025-06-02T10:15:00Z", "2025-06-02T10:30:00Z"] {
        parts.clock.set(ts(t));
        assert_eq!(scheduler.tick(parts.clock.now()).fired.len(), 1);
    }
    // Pull the plug while the original store is still open.
    let crashed = fs.crash_copy(&Crash::PowerLoss);
    let recovered = parts.scheduler(open_log(&crashed));
    let sync = recovered.get(&id("sync")).unwrap();
    assert_eq!(sync.fire_count, 2);
    assert_eq!(sync.last_fire_at, Some(ts("2025-06-02T10:30:00Z")));
    assert_eq!(sync.next_fire_at, Some(ts("2025-06-02T10:45:00Z")));
}

#[test]
fn a_running_workflow_continues_after_reopening() {
    let fs = MemFs::new();
    let parts = Parts::new("2025-05-05T10:00:00Z");
    let mut spec = workflow(
        "crawl",
        vec![
            step("fetch", &[]),
            step("left", &["fetch"]),
            step("join", &["left"]),
        ],
    );
    spec.steps[2].job.payload = json!({"source": "{{steps.fetch.result.url}}"});
    let run = {
        let engine = parts.engine(open_log(&fs));
        let run = engine.submit(spec).unwrap();
        let fetch = parts.queue.find_step(run.id, "fetch").unwrap();
        let done = parts
            .queue
            .succeed(&fetch.id, json!({"url": "http://a/b"}))
            .unwrap();
        engine.on_job_finished(&done).unwrap().unwrap();
        run
    };
    let engine = parts.engine(open_log(&fs));
    assert_eq!(engine.running(), 1);
    assert_eq!(engine.recover().unwrap(), 0, "nothing was missed");
    let reloaded = engine.get(run.id).unwrap().unwrap();
    assert_eq!(reloaded.steps["fetch"].state, StepState::Succeeded);
    assert_eq!(reloaded.steps["left"].state, StepState::Running);

    let left = parts.queue.find_step(run.id, "left").unwrap();
    let done = parts.queue.succeed(&left.id, Value::Null).unwrap();
    engine.on_job_finished(&done).unwrap().unwrap();
    // The result stored before the restart feeds the placeholder.
    let join = parts.queue.find_step(run.id, "join").unwrap();
    assert_eq!(join.spec.payload, json!({"source": "http://a/b"}));
}

#[test]
fn finished_runs_are_read_back_from_the_store() {
    let fs = MemFs::new();
    let parts = Parts::new("2025-05-05T10:00:00Z");
    let run = {
        let engine = parts.engine(open_log(&fs));
        let run = engine
            .submit(workflow("one", vec![step("only", &[])]))
            .unwrap();
        let job = parts.queue.find_step(run.id, "only").unwrap();
        let dead = parts.queue.fail(&job.id, "exit status 2").unwrap();
        engine.on_job_finished(&dead).unwrap().unwrap();
        run
    };
    let engine = parts.engine(open_log(&fs));
    assert_eq!(engine.running(), 0);
    let details = engine.details(run.id).unwrap().unwrap();
    assert_eq!(details.workflow.state, WorkflowState::Failed);
    assert!(details
        .error("only")
        .unwrap()
        .ends_with("is dead: exit status 2"));
}

#[test]
fn recover_picks_up_a_completion_missed_while_down() {
    let f = Flow::new();
    let mut spec = workflow("etl", vec![step("a", &[]), step("b", &["a"])]);
    spec.steps[1].job.payload = json!("{{steps.a.result.rows}}");
    let run = f.submit(spec);
    // The job finishes while the engine is not listening.
    f.queue
        .succeed(&f.job(run.id, "a").id, json!({"rows": 12}))
        .unwrap();
    let f = f.restart();
    assert_eq!(f.engine.recover().unwrap(), 1);
    let after = f.get(run.id);
    assert_eq!(after.steps["a"].state, StepState::Succeeded);
    assert_eq!(after.steps["b"].state, StepState::Running);
    assert_eq!(f.job(run.id, "b").spec.payload, json!(12));
}

/// A dispatcher that only knows job states, not job records.
struct StatesOnly(Arc<MemoryDispatcher>);

impl Dispatcher for StatesOnly {
    fn dispatch(&self, spec: &JobSpec) -> Result<DispatchOutcome, DispatchError> {
        self.0.dispatch(spec)
    }

    fn job_state(&self, id: &JobId) -> Option<JobState> {
        self.0.job_state(id)
    }

    fn cancel(&self, id: &JobId) -> Result<(), DispatchError> {
        self.0.cancel(id)
    }
}

#[test]
fn recovery_without_job_records_cannot_feed_placeholders() {
    quiet();
    let clock = ManualClock::new(ts("2025-05-05T10:00:00Z"));
    let queue = Arc::new(MemoryDispatcher::new(clock.shared()));
    let kv: Arc<dyn Kv> = Arc::new(MemKv::new());
    let open = || {
        WorkflowEngine::open(
            Arc::clone(&kv),
            clock.shared(),
            Arc::new(StatesOnly(queue.clone())),
            Arc::new(CollectingSink::new()),
        )
        .unwrap()
    };
    let mut spec = workflow("etl", vec![step("a", &[]), step("b", &["a"])]);
    spec.steps[1].job.payload = json!({"rows": "{{steps.a.result.rows}}"});
    let run = open().submit(spec).unwrap();
    let a = queue.find_step(run.id, "a").unwrap();
    queue.succeed(&a.id, json!({"rows": 12})).unwrap();
    let engine = open();
    assert_eq!(engine.recover().unwrap(), 1);
    let details = engine.details(run.id).unwrap().unwrap();
    assert_eq!(details.workflow.steps["a"].state, StepState::Succeeded);
    assert_eq!(details.workflow.steps["b"].state, StepState::Failed);
    assert_eq!(
        details.error("b"),
        Some(
            r#"placeholder {{steps.a.result.rows}}: the result of step "a" is not available (its completion was recovered without the job record)"#
        )
    );
}

#[test]
fn recover_fails_steps_whose_jobs_vanished() {
    let f = Flow::new();
    let run = f.submit(workflow("etl", vec![step("a", &[])]));
    let a = f.job(run.id, "a").id;
    f.queue.forget(&a);
    assert_eq!(f.engine.recover().unwrap(), 1);
    let details = f.engine.details(run.id).unwrap().unwrap();
    assert_eq!(details.workflow.state, WorkflowState::Failed);
    assert_eq!(
        details.error("a"),
        Some(format!("job {a} no longer exists").as_str())
    );
}

#[test]
fn a_crash_between_dispatch_and_save_is_recovered_without_a_second_job() {
    let flaky = FlakyKv::new(Arc::new(MemKv::new()));
    let f = Flow::with(flaky.clone());
    let run = f.submit(workflow("etl", vec![step("a", &[]), step("b", &["a"])]));
    let a = f
        .queue
        .succeed(&f.job(run.id, "a").id, Value::Null)
        .unwrap();
    // b is dispatched, then saving fails: the process dies here.
    flaky.refuse_writes(true);
    assert!(f.engine.on_job_finished(&a).is_err());
    let b = f.job(run.id, "b").id;
    flaky.refuse_writes(false);

    let f = f.restart();
    assert_eq!(
        f.get(run.id).steps["a"].state,
        StepState::Running,
        "nothing was saved"
    );
    assert_eq!(f.engine.recover().unwrap(), 1);
    let after = f.get(run.id);
    assert_eq!(after.steps["a"].state, StepState::Succeeded);
    assert_eq!(after.steps["b"].state, StepState::Running);
    assert_eq!(
        after.steps["b"].job,
        Some(b),
        "the original job was adopted"
    );
    assert_eq!(f.queue.job_count(), 2);
}

#[test]
fn a_run_interrupted_during_submit_is_finished_by_recovery() {
    let flaky = FlakyKv::new(Arc::new(MemKv::new()));
    let f = Flow::with(flaky.clone());
    // The run is saved, its root is dispatched, then the process dies.
    flaky.fail_after(1);
    assert!(f
        .engine
        .submit(workflow("etl", vec![step("a", &[])]))
        .is_err());
    let job: Job = f.queue.last().unwrap();
    let run = job.spec.workflow.clone().unwrap().id;
    flaky.refuse_writes(false);

    let f = f.restart();
    assert_eq!(f.get(run).steps["a"].state, StepState::Pending);
    assert_eq!(f.engine.recover().unwrap(), 1);
    assert_eq!(f.get(run).steps["a"].job, Some(job.id));
    assert_eq!(f.queue.job_count(), 1);
    f.clock.advance(Duration::from_secs(1));
    let done = f.succeed(run, "a", Value::Null);
    assert_eq!(done.state, WorkflowState::Succeeded);
}

#[test]
fn a_duplicate_that_already_finished_is_taken_as_finished() {
    let flaky = FlakyKv::new(Arc::new(MemKv::new()));
    let f = Flow::with(flaky.clone());
    flaky.fail_after(1);
    assert!(f
        .engine
        .submit(workflow("etl", vec![step("a", &[]), step("b", &["a"])]))
        .is_err());
    let a = f.queue.last().unwrap();
    let run = a.spec.workflow.clone().unwrap().id;
    // The job even finished before belay came back.
    f.queue.succeed(&a.id, json!("done")).unwrap();
    flaky.refuse_writes(false);

    let f = f.restart();
    assert_eq!(f.engine.recover().unwrap(), 1);
    let after = f.get(run);
    assert_eq!(after.steps["a"].state, StepState::Succeeded);
    assert_eq!(after.steps["b"].state, StepState::Running);
}
