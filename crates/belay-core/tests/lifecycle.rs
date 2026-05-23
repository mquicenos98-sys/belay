//! Starting, stopping and restarting engines: durability on disk,
//! recovery of workflows, checkpoints, shutdown, and the background
//! threads in real time.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use belay_common::fs::TempDir;
use belay_common::{Clock, ManualClock, Timestamp};
use belay_core::{Config, Engine, Error, Ownership, Paging};
use belay_json::json;
use belay_proto::api::{CompleteRequest, ErrorCode, LeaseRequest};
use belay_proto::event::EventFilter;
use belay_proto::workflow::{StepSpec, StepState, WorkflowState};
use belay_proto::{JobSource, JobSpec, JobState, QueueName, ScheduleSpec, WorkerId, WorkflowSpec};
use belay_queue::{EngineOptions, QueueEngine};
use belay_store::{LogKv, Options};
use belay_worker::{builtin, WorkerConfig};
use common::{eventually, quiet_logs, START};

const CONFIG: &str = r#"
[queues.emails]
max_concurrency = 3

[[schedules]]
id = "nightly"
cron = "0 3 * * *"
template = { queue = "reports", kind = "nightly" }
"#;

fn disk_config(dir: &Path) -> Config {
    let mut config = Config::from_str(CONFIG).unwrap();
    config.storage.dir = dir.to_path_buf();
    config
}

fn start(config: Config, clock: &ManualClock) -> Engine {
    quiet_logs();
    Engine::builder()
        .config(config)
        .clock(clock.shared())
        .background(false)
        .start()
        .unwrap_or_else(|e| panic!("engine did not start: {e}"))
}

fn clock() -> ManualClock {
    ManualClock::new(START.parse::<Timestamp>().unwrap())
}

fn two_steps() -> WorkflowSpec {
    WorkflowSpec::new("pipeline")
        .step(StepSpec::new("extract", JobSpec::new("wf", "extract")))
        .step(StepSpec::new("load", JobSpec::new("wf", "load")).after("extract"))
}

fn lease_one(engine: &Engine, queue: &'static str) -> belay_proto::api::LeasedJob {
    let req = LeaseRequest::new(
        WorkerId::from_static("w1"),
        vec![QueueName::from_static(queue)],
    );
    let mut jobs = engine.lease(&req).unwrap();
    assert_eq!(jobs.len(), 1, "expected one job in {queue}");
    jobs.remove(0)
}

#[test]
fn a_restart_keeps_jobs_schedules_and_workflows() {
    let dir = TempDir::new().unwrap();
    let clock = clock();
    let (job, run) = {
        let engine = start(disk_config(dir.path()), &clock);
        let job = engine.enqueue(JobSpec::new("emails", "send")).unwrap().id;
        engine
            .create_schedule(ScheduleSpec::new(
                "from-api".parse().unwrap(),
                "*/15 * * * *",
                JobSpec::new("q", "k"),
            ))
            .unwrap();
        let run = engine.submit_workflow(two_steps()).unwrap().id;
        let extract = lease_one(&engine, "wf");
        engine
            .complete(&extract.job.id, &CompleteRequest::new(extract.token))
            .unwrap();
        (job, run)
    };

    clock.advance(Duration::from_secs(60));
    let engine = start(disk_config(dir.path()), &clock);
    assert_eq!(engine.get_job(&job).unwrap().state, JobState::Ready);
    let emails = engine
        .queue_info(&QueueName::from_static("emails"))
        .unwrap();
    assert_eq!(emails.config.max_concurrency, Some(3));
    let ids: Vec<String> = engine
        .list_schedules(&Paging::new())
        .unwrap()
        .items
        .iter()
        .map(|s| s.id().to_string())
        .collect();
    assert_eq!(ids, ["from-api", "nightly"]);
    let workflow = engine.get_workflow(run).unwrap();
    assert_eq!(workflow.steps["extract"].state, StepState::Succeeded);
    assert_eq!(workflow.steps["load"].state, StepState::Running);

    let load = lease_one(&engine, "wf");
    engine
        .complete(&load.job.id, &CompleteRequest::new(load.token))
        .unwrap();
    assert_eq!(
        engine.get_workflow(run).unwrap().state,
        WorkflowState::Succeeded
    );
}

#[test]
fn an_injected_memory_store_survives_the_engine() {
    let kv: Arc<dyn belay_store::Kv> = Arc::new(belay_store::MemKv::new());
    let clock = clock();
    let open = || {
        quiet_logs();
        Engine::builder()
            .config(Config::dev())
            .clock(clock.shared())
            .kv(Arc::clone(&kv))
            .background(false)
            .start()
            .unwrap()
    };
    let (job, run) = {
        let engine = open();
        assert_eq!(engine.info().storage, "custom");
        let job = engine.enqueue(JobSpec::new("q", "k")).unwrap().id;
        let run = engine.submit_workflow(two_steps()).unwrap().id;
        engine
            .create_schedule(ScheduleSpec::new(
                "memory".parse().unwrap(),
                "@daily",
                JobSpec::new("q", "daily"),
            ))
            .unwrap();
        (job, run)
    };
    let engine = open();
    assert_eq!(engine.get_job(&job).unwrap().kind(), "k");
    assert_eq!(
        engine.get_workflow(run).unwrap().state,
        WorkflowState::Running
    );
    assert!(engine.get_schedule(&"memory".parse().unwrap()).is_ok());
    assert!(engine.store_report().dir.is_none());
}

#[test]
fn steps_finished_while_the_engine_was_down_are_picked_up_at_start() {
    let dir = TempDir::new().unwrap();
    let clock = clock();
    let run = {
        let engine = start(disk_config(dir.path()), &clock);
        engine.submit_workflow(two_steps()).unwrap().id
    };
    // A bare queue engine finishes the first step: no workflow engine is
    // listening, as after a crash between the two.
    {
        let kv = Arc::new(LogKv::open(dir.path(), Options::new()).unwrap());
        let queue = QueueEngine::open(
            kv.clone(),
            clock.shared(),
            belay_proto::source::NoopSink::shared(),
            EngineOptions::new(),
        )
        .unwrap();
        let req = LeaseRequest::new(
            WorkerId::from_static("w1"),
            vec![QueueName::from_static("wf")],
        );
        let leased = queue.lease(&req).unwrap().remove(0);
        let mut done = CompleteRequest::new(leased.token);
        done.result = Some(json!({"rows": 7}));
        queue.complete(&leased.job.id, &done).unwrap();
        kv.close().unwrap();
    }
    let engine = start(disk_config(dir.path()), &clock);
    let workflow = engine.get_workflow(run).unwrap();
    assert_eq!(workflow.steps["extract"].state, StepState::Succeeded);
    assert_eq!(workflow.steps["load"].state, StepState::Running);
    assert_eq!(lease_one(&engine, "wf").job.spec.kind, "load");
}

#[test]
fn shutdown_is_orderly_idempotent_and_releases_the_store() {
    let dir = TempDir::new().unwrap();
    let clock = clock();
    let first = Arc::new(start(disk_config(dir.path()), &clock));
    let sub = first.subscribe(EventFilter::new());
    first.enqueue(JobSpec::new("q", "k")).unwrap();
    assert!(first.is_ready());

    let held = Arc::clone(&first);
    first.shutdown();
    first.shutdown();
    assert!(held.is_shutting_down());
    assert!(!held.is_ready());

    // Everything queued is still delivered, then the stream ends.
    while sub.try_recv().is_ok() {}
    assert_eq!(sub.try_recv(), Err(belay_core::RecvError::Closed));

    let req = LeaseRequest::new(
        WorkerId::from_static("w1"),
        vec![QueueName::from_static("q")],
    );
    assert_eq!(held.lease(&req).unwrap_err().code, ErrorCode::Unavailable);

    // The directory lock is gone once the last handle is dropped.
    drop(held);
    drop(first);
    let second = start(disk_config(dir.path()), &clock);
    assert_eq!(
        second
            .queue_info(&QueueName::from_static("q"))
            .unwrap()
            .counts
            .ready,
        1
    );
}

#[test]
fn a_locked_store_or_a_bad_configured_schedule_fails_the_start() {
    let dir = TempDir::new().unwrap();
    let clock = clock();
    let _running = start(disk_config(dir.path()), &clock);
    let err = Engine::builder()
        .config(disk_config(dir.path()))
        .clock(clock.shared())
        .background(false)
        .start()
        .unwrap_err();
    assert!(matches!(err, Error::Store(_)), "{err}");

    let mut config = Config::dev();
    config.schedules = Config::from_str(
        "[[schedules]]\nid = \"s\"\ncron = \"0 * * * *\"\ncalendar = \"missing\"\ntemplate = { queue = \"q\", kind = \"k\" }\n",
    )
    .unwrap()
    .schedules;
    let err = Engine::builder()
        .config(config)
        .background(false)
        .start()
        .unwrap_err();
    assert!(matches!(err, Error::Apply { .. }), "{err}");
    assert!(
        err.to_string()
            .starts_with("applying schedule s from the configuration failed"),
        "{err}"
    );
}

#[test]
fn checkpoints_happen_on_demand_and_on_the_interval() {
    let dir = TempDir::new().unwrap();
    let clock = clock();
    let engine = start(disk_config(dir.path()), &clock);
    engine.enqueue(JobSpec::new("q", "k")).unwrap();
    let response = engine.checkpoint().unwrap();
    assert!(response.seq > 0);
    let report = engine.store_report();
    assert_eq!(report.storage, "disk");
    assert_eq!(report.dir.as_deref(), Some(dir.path()));
    assert_eq!(report.stats.checkpoints, 1);

    engine.enqueue(JobSpec::new("q", "k2")).unwrap();
    let early = engine.tick(clock.advance(Duration::from_secs(1800)));
    assert!(early.checkpoint.is_none());
    let due = engine.tick(clock.advance(Duration::from_secs(1800)));
    assert!(due.checkpoint.is_some_and(|c| c.written));
    assert_eq!(engine.store_report().stats.checkpoints, 2);
    let json = belay_json::ToJson::to_json(&engine.store_report());
    assert_eq!(json["storage"], "disk");
    assert_eq!(json["recovery"]["lost_data"], false);
}

#[test]
fn a_single_node_reports_itself() {
    let clock = clock();
    let engine = start(Config::dev(), &clock);
    clock.advance(Duration::from_secs(90));
    let info = engine.info();
    assert_eq!(info.version, belay_core::VERSION);
    assert_eq!(info.storage, "memory");
    assert_eq!(info.cluster_size, 1);
    assert_eq!(info.uptime, Duration::from_secs(90));
    assert_eq!(info.node, engine.node_id());
    let members = engine.members();
    assert_eq!(members.len(), 1);
    assert!(members[0].leader);
    assert_eq!(members[0].addr, "http://127.0.0.1:7700");
    assert_eq!(
        engine.owner_of(&QueueName::from_static("any")),
        Ownership::Local
    );
    assert!(!engine.is_clustered());
    assert!(engine.is_joined());
    engine.enqueue(JobSpec::new("q", "k")).unwrap();
    assert!(engine.verify().unwrap().is_consistent());
}

#[test]
fn background_threads_and_an_embedded_worker_run_jobs_in_real_time() {
    quiet_logs();
    let mut config = Config::dev();
    config.engine.tick = Duration::from_millis(10);
    config.scheduler.tick = Duration::from_millis(20);
    config.workflows.sweep_every = Duration::from_millis(20);
    config.metrics.refresh = Duration::from_millis(20);
    let engine = Engine::start(config).unwrap();

    let delayed = engine
        .enqueue(JobSpec::new("default", "echo").delay(Duration::from_millis(30)))
        .unwrap()
        .id;
    let run = engine
        .submit_workflow(
            WorkflowSpec::new("echoes")
                .step(StepSpec::new(
                    "one",
                    JobSpec::new("default", "echo").payload(json!({"n": 1})),
                ))
                .step(StepSpec::new("two", JobSpec::new("default", "echo")).after("one")),
        )
        .unwrap()
        .id;
    engine
        .create_schedule(ScheduleSpec::new(
            "every-second".parse().unwrap(),
            "* * * * * *",
            JobSpec::new("default", "echo"),
        ))
        .unwrap();

    let worker = engine
        .embedded_worker(
            builtin::defaults(),
            WorkerConfig::default()
                .queues(vec![QueueName::from_static("default")])
                .poll_wait(Duration::from_millis(50)),
        )
        .unwrap();
    let finished = eventually(Duration::from_secs(10), || {
        engine
            .get_job(&delayed)
            .is_ok_and(|j| j.state == JobState::Succeeded)
            && engine
                .get_workflow(run)
                .is_ok_and(|w| w.state == WorkflowState::Succeeded)
            && engine
                .get_schedule(&"every-second".parse().unwrap())
                .is_ok_and(|s| s.fire_count >= 1)
    });
    assert!(finished, "jobs, workflow and schedule did not all complete");
    let report = worker.stop();
    assert!(report.stats.succeeded >= 3);

    let began = Instant::now();
    engine.shutdown();
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "shutdown took {:?}",
        began.elapsed()
    );
    assert!(engine.clock().now() >= engine.info().started_at);
}
