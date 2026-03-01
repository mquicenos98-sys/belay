//! End-to-end flows through one engine on a manual clock: jobs, retries,
//! schedules, workflows, the event stream and metrics.

mod common;

use std::time::Duration;

use belay_common::BackoffPolicy;
use belay_core::{Config, Delivery, Paging};
use belay_json::json;
use belay_proto::api::{ErrorCode, HeartbeatRequest, JobFilter};
use belay_proto::event::{EventFilter, EventKind, EventPattern};
use belay_proto::workflow::{StepSpec, StepState, WorkflowState};
use belay_proto::{
    JobSource, JobSpec, JobState, QueueConfig, QueueName, ScheduleSpec, WorkflowSpec,
};
use common::{everything, Harness};

const MINUTE: Duration = Duration::from_secs(60);

const HOUR: Duration = Duration::from_secs(3600);

#[test]
fn enqueue_lease_complete() {
    let h = Harness::new();
    let first_seq = h.engine.events().next_seq();
    let id = h
        .engine
        .enqueue(JobSpec::new("emails", "send_welcome").payload(json!({"user": 42})))
        .unwrap()
        .id;
    assert_eq!(h.engine.get_job(&id).unwrap().state, JobState::Ready);

    let leased = h.lease_one("emails");
    assert_eq!(leased.job.id, id);
    assert_eq!(leased.job.spec.payload["user"], 42);
    let beat = h
        .engine
        .heartbeat(&id, &HeartbeatRequest::new(leased.token))
        .unwrap();
    assert!(!beat.cancel_requested);

    h.clock.advance(Duration::from_secs(2));
    h.complete(&leased, Some(json!({"sent": true})));
    let job = h.engine.get_job(&id).unwrap();
    assert_eq!(job.state, JobState::Succeeded);
    assert_eq!(job.result, Some(json!({"sent": true})));
    assert_eq!(
        h.event_types(first_seq),
        [
            "job.enqueued",
            "worker.seen",
            "job.started",
            "job.succeeded"
        ]
    );

    // The same token cannot report twice.
    let again = h
        .engine
        .complete(&id, &belay_proto::api::CompleteRequest::new(leased.token));
    assert_eq!(again.unwrap_err().code, ErrorCode::LeaseLost);
}

#[test]
fn failed_attempts_retry_with_backoff_then_die() {
    let h = Harness::new();
    let spec = JobSpec::new("imports", "fetch")
        .max_attempts(2)
        .retry("const:10s".parse::<BackoffPolicy>().unwrap());
    let id = h.engine.enqueue(spec).unwrap().id;

    let first = h.lease_one("imports");
    h.fail(&first, "upstream 503", true);
    let job = h.engine.get_job(&id).unwrap();
    assert_eq!(job.state, JobState::Retrying);
    assert!(
        h.lease("imports", 1).is_empty(),
        "not before the backoff delay"
    );

    let report = h.advance(Duration::from_secs(10));
    assert_eq!(report.queue.promoted, 1);
    let second = h.lease_one("imports");
    assert_eq!(second.job.attempt, 2);
    h.fail(&second, "upstream 503 again", true);

    let job = h.engine.get_job(&id).unwrap();
    assert_eq!(job.state, JobState::Dead);
    assert_eq!(job.errors.len(), 2);
    let types = h.event_types(0);
    assert!(types.ends_with(&["job.started", "job.dead"]), "{types:?}");

    // An operator sends it back.
    let retried = h.engine.retry_job(&id).unwrap();
    assert_eq!(retried.state, JobState::Ready);
}

#[test]
fn an_expired_lease_counts_as_a_failed_attempt() {
    let h = Harness::new();
    let id = h
        .engine
        .enqueue(JobSpec::new("slow", "crunch").max_attempts(1))
        .unwrap()
        .id;
    let leased = h.lease_one("slow");
    let report = h.advance(Duration::from_secs(31));
    assert_eq!(report.queue.expired, 1);
    assert_eq!(h.engine.get_job(&id).unwrap().state, JobState::Dead);
    let err = h
        .engine
        .heartbeat(&id, &HeartbeatRequest::new(leased.token))
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::LeaseLost);
}

#[test]
fn operators_cancel_promote_and_delete() {
    let h = Harness::new();
    let later = h
        .engine
        .enqueue(JobSpec::new("q", "later").delay(HOUR))
        .unwrap();
    assert_eq!(later.state, JobState::Scheduled);
    assert_eq!(
        h.engine.promote_job(&later.id).unwrap().state,
        JobState::Ready
    );
    assert_eq!(
        h.engine.cancel_job(&later.id).unwrap().state,
        JobState::Cancelled
    );
    h.engine.delete_job(&later.id).unwrap();
    assert_eq!(
        h.engine.get_job(&later.id).unwrap_err().code,
        ErrorCode::NotFound
    );

    let running = h.engine.enqueue(JobSpec::new("q", "busy")).unwrap().id;
    h.lease_one("q");
    assert_eq!(
        h.engine.delete_job(&running).unwrap_err().code,
        ErrorCode::FailedPrecondition
    );
}

#[test]
fn queues_are_listed_configured_paused_and_purged() {
    let h = Harness::new();
    for queue in ["b", "a", "c"] {
        h.engine.enqueue(JobSpec::new(queue, "k")).unwrap();
    }
    let page = h.engine.list_queues(&Paging::new().limit(2)).unwrap();
    let names: Vec<&str> = page.items.iter().map(|q| q.name.as_str()).collect();
    assert_eq!(names, ["a", "b"]);
    let rest = h
        .engine
        .list_queues(&Paging::new().after(page.next_cursor.clone().unwrap()))
        .unwrap();
    assert_eq!(rest.items[0].name, "c");

    let a = QueueName::from_static("a");
    let config = QueueConfig {
        max_concurrency: Some(1),
        ..QueueConfig::default()
    };
    let info = h.engine.set_queue_config(&a, config).unwrap();
    assert_eq!(info.config.max_concurrency, Some(1));
    assert!(h.engine.pause_queue(&a).unwrap().paused);
    assert!(h.lease("a", 1).is_empty());
    assert!(!h.engine.resume_queue(&a).unwrap().paused);
    assert_eq!(
        h.engine
            .purge_queue(&a, Some(JobState::Ready))
            .unwrap()
            .purged,
        1
    );
    assert_eq!(h.engine.delete_queue(&a, false).unwrap().purged, 0);
    assert_eq!(
        h.engine.queue_info(&a).unwrap_err().code,
        ErrorCode::NotFound
    );
}

#[test]
fn a_cron_schedule_fires_into_the_queue() {
    let h = Harness::new();
    let spec = ScheduleSpec::new(
        "hourly-report".parse().unwrap(),
        "0 * * * *",
        JobSpec::new("reports", "build").payload(json!({"format": "pdf"})),
    );
    let schedule = h.engine.create_schedule(spec.clone()).unwrap();
    assert_eq!(
        schedule.next_fire_at.unwrap().to_string(),
        "2025-03-03T09:00:00.000Z"
    );
    assert_eq!(
        h.engine.create_schedule(spec).unwrap_err().code,
        ErrorCode::AlreadyExists
    );

    // Nothing before the hour.
    assert!(h.advance(59 * MINUTE).schedules.fired.is_empty());
    let report = h.advance(MINUTE);
    assert_eq!(report.schedules.fired.len(), 1);
    let leased = h.lease_one("reports");
    assert_eq!(leased.job.spec.meta["belay.schedule"], "hourly-report");
    assert_eq!(leased.job.spec.payload["format"], "pdf");

    let id = "hourly-report".parse().unwrap();
    let preview = h.engine.preview_schedule(&id, 3).unwrap();
    let times: Vec<String> = preview.times.iter().map(|t| t.to_string()).collect();
    assert_eq!(times[0], "2025-03-03T10:00:00.000Z");
    assert_eq!(times.len(), 3);

    let triggered = h.engine.trigger_schedule(&id).unwrap();
    assert_eq!(
        h.engine.get_job(&triggered.job).unwrap().state,
        JobState::Ready
    );
    assert_eq!(h.engine.get_schedule(&id).unwrap().fire_count, 2);

    h.engine.pause_schedule(&id).unwrap();
    assert!(h.advance(HOUR).schedules.fired.is_empty());
    h.engine.resume_schedule(&id).unwrap();
    assert_eq!(h.advance(HOUR).schedules.fired.len(), 1);

    let listed = h.engine.list_schedules(&Paging::new()).unwrap();
    assert_eq!(listed.items.len(), 1);
    h.engine.delete_schedule(&id).unwrap();
    assert_eq!(
        h.engine.get_schedule(&id).unwrap_err().code,
        ErrorCode::NotFound
    );
}

#[test]
fn configured_calendars_and_schedules_apply_at_start() {
    let config = Config::from_str(
        r#"
        [storage]
        in_memory = true

        [[calendars]]
        name = "weekdays"
        exclude_weekdays = ["sat", "sun"]

        [[schedules]]
        id = "weekday-sync"
        cron = "0 9 * * *"
        calendar = "weekdays"
        template = { queue = "sync", kind = "sync" }
        "#,
    )
    .unwrap();
    let h = Harness::with_config(config);
    assert_eq!(h.engine.get_calendar("weekdays").unwrap().name, "weekdays");
    let id = "weekday-sync".parse().unwrap();
    let schedule = h.engine.get_schedule(&id).unwrap();
    assert_eq!(
        schedule.next_fire_at.unwrap().to_string(),
        "2025-03-03T09:00:00.000Z"
    );
    // Monday 09:00 fires; the weekend does not.
    assert_eq!(h.advance(HOUR).schedules.fired.len(), 1);
    let fired: usize = (0..6)
        .map(|_| h.advance(24 * HOUR).schedules.fired.len())
        .sum();
    assert_eq!(fired, 4, "Tuesday to Friday");
    assert_eq!(
        h.engine.delete_calendar("weekdays").unwrap_err().code,
        ErrorCode::FailedPrecondition
    );
}

fn three_steps() -> WorkflowSpec {
    WorkflowSpec::new("etl")
        .step(StepSpec::new(
            "fetch",
            JobSpec::new("etl", "fetch").payload(json!({"source": "s3://bucket/in"})),
        ))
        .step(
            StepSpec::new(
                "transform",
                JobSpec::new("etl", "transform")
                    .payload(json!({"input": "{{steps.fetch.result.path}}"})),
            )
            .after("fetch"),
        )
        .step(
            StepSpec::new(
                "store",
                JobSpec::new("etl", "store")
                    .payload(json!({"rows": "{{steps.transform.result.rows}}"})),
            )
            .after("transform"),
        )
}

#[test]
fn a_three_step_workflow_advances_through_queue_completions() {
    let h = Harness::new();
    let run = h.engine.submit_workflow(three_steps()).unwrap();
    assert_eq!(run.state, WorkflowState::Running);
    assert_eq!(run.steps["fetch"].state, StepState::Running);
    assert_eq!(run.steps["transform"].state, StepState::Pending);

    let fetch = h.lease_one("etl");
    assert_eq!(fetch.job.spec.kind, "fetch");
    h.complete(&fetch, Some(json!({"path": "/tmp/in.csv"})));

    let transform = h.lease_one("etl");
    assert_eq!(transform.job.spec.kind, "transform");
    assert_eq!(transform.job.spec.payload["input"], "/tmp/in.csv");
    assert_eq!(
        h.engine.get_workflow(run.id).unwrap().steps["transform"].state,
        StepState::Running
    );
    h.complete(&transform, Some(json!({"rows": 1200})));

    let store = h.lease_one("etl");
    assert_eq!(store.job.spec.payload["rows"], 1200);
    h.complete(&store, None);

    let done = h.engine.get_workflow(run.id).unwrap();
    assert_eq!(done.state, WorkflowState::Succeeded);
    assert!(done.finished_at.is_some());
    let workflow_events: Vec<&str> = h
        .event_types(0)
        .into_iter()
        .filter(|t| t.starts_with("workflow."))
        .collect();
    assert_eq!(workflow_events.first(), Some(&"workflow.started"));
    assert_eq!(workflow_events.last(), Some(&"workflow.finished"));

    let page = h
        .engine
        .list_workflows(&belay_core::WorkflowFilter::new().state(WorkflowState::Succeeded))
        .unwrap();
    assert_eq!(page.items.len(), 1);

    // Finished runs are kept for workflows.keep_for, then swept.
    let report = h.advance(7 * 24 * HOUR);
    assert_eq!(report.workflows_deleted, 1);
    assert_eq!(
        h.engine.get_workflow(run.id).unwrap_err().code,
        ErrorCode::NotFound
    );
}

#[test]
fn a_dead_step_fails_the_workflow_and_skips_the_rest() {
    let h = Harness::new();
    let spec = WorkflowSpec::new("fragile")
        .step(StepSpec::new(
            "first",
            JobSpec::new("wf", "a").max_attempts(1),
        ))
        .step(StepSpec::new("second", JobSpec::new("wf", "b")).after("first"));
    let run = h.engine.submit_workflow(spec).unwrap();
    let first = h.lease_one("wf");
    h.fail(&first, "bad input", false);
    let run = h.engine.get_workflow(run.id).unwrap();
    assert_eq!(run.state, WorkflowState::Failed);
    assert_eq!(run.steps["first"].state, StepState::Failed);
    assert_ne!(run.steps["second"].state, StepState::Running);
    assert!(h.lease("wf", 1).is_empty());
    let details = h.engine.workflow_details(run.id).unwrap();
    assert!(details
        .error("first")
        .is_some_and(|e| e.contains("bad input")));
}

#[test]
fn cancelling_a_workflow_cancels_its_ready_jobs() {
    let h = Harness::new();
    let run = h.engine.submit_workflow(three_steps()).unwrap();
    let fetch_job = run.steps["fetch"].job.unwrap();
    let cancelled = h.engine.cancel_workflow(run.id).unwrap();
    assert_eq!(cancelled.state, WorkflowState::Cancelled);
    assert_eq!(
        h.engine.get_job(&fetch_job).unwrap().state,
        JobState::Cancelled
    );
    assert!(h.lease("etl", 1).is_empty());
}

#[test]
fn subscribers_replay_then_follow_the_stream() {
    let h = Harness::new();
    let start = h.engine.events().next_seq();
    h.engine.enqueue(JobSpec::new("a", "k")).unwrap();
    let sub = h.engine.subscribe(
        EventFilter::new()
            .from_seq(start)
            .with_type(EventPattern::Category("job")),
    );
    h.engine.enqueue(JobSpec::new("b", "k")).unwrap();
    let mut seen = Vec::new();
    while let Ok(Delivery::Event(e)) = sub.try_recv() {
        seen.push((e.seq - start, e.kind.queue().map(|q| q.to_string())));
    }
    assert_eq!(seen, [(0, Some("a".into())), (1, Some("b".into()))]);
    // Sequence numbers start from the engine's start time, so they keep
    // growing across restarts.
    assert!(start >= (1_740_988_800u64 << 20));
}

#[test]
fn a_slow_subscriber_is_told_how_much_it_missed() {
    let mut config = Config::dev();
    config.events.buffer = 4;
    config.events.subscriber_queue = 2;
    let h = Harness::with_config(config);
    let sub = h.engine.events().subscribe_live(everything());
    for i in 0..10 {
        h.engine
            .enqueue(JobSpec::new("q", format!("k{i}")))
            .unwrap();
    }
    let mut deliveries = Vec::new();
    while let Ok(d) = sub.try_recv() {
        deliveries.push(match d {
            Delivery::Event(e) => format!("event:{}", e.type_name()),
            Delivery::Lagged { missed } => format!("lagged:{missed}"),
        });
    }
    // Two queued, then the gap back to the four still buffered.
    assert_eq!(deliveries.len(), 7, "{deliveries:?}");
    assert_eq!(deliveries[2], "lagged:4");
    assert_eq!(h.engine.events().stats().lagged, 1);
}
