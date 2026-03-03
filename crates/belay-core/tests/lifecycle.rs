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
