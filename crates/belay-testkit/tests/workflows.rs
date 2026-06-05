//! Workflows submitted over HTTP and run by a worker: a fan-out/fan-in
//! graph whose payloads are templated on earlier results, a failing step
//! under each failure policy, conditions routing around a step whose
//! template cannot be filled, a run surviving a server restart, and a run
//! cancelled while a step works.

use belay_proto::workflow::Condition;
use belay_testkit::prelude::*;

const QUEUE: &str = "etl";

fn etl(kind: &str) -> JobSpec {
    job(QueueName::from_static(QUEUE), kind)
}

fn step_states(run: &belay_proto::Workflow) -> Vec<(String, StepState)> {
    run.steps
        .iter()
        .map(|(name, status)| (name.clone(), status.state))
        .collect()
}

#[test]
fn a_fan_out_fan_in_workflow_passes_results_down_through_templates() -> Result<()> {
    let server = TestServer::start()?;
    let events = server.event_log()?;
    let worker = server
        .worker()
        .queue(QUEUE)
        .recording("fetch")
        .recording("split")
        .recording("merge")
        .start()?;
    let client = server.client();

    let fetch = etl("fetch").payload(json!({
        "url": "https://example.com/export.csv",
        "rows": 3,
    }));
    let split = |part: u64| {
        etl("split").payload(json!({
            "source": placeholder("fetch", ".url"),
            "rows": placeholder("fetch", "/rows"),
            "part": part,
        }))
    };
    let merge = etl("merge").payload(json!({
        "parts": [
            placeholder("split-a", ".part"),
            placeholder("split-b", ".part"),
            placeholder("split-c", ".part"),
        ],
        "summary": format!(
            "merged {} rows from {}",
            placeholder("fetch", ".rows"),
            placeholder("fetch", ".url")
        ),
    }));
    let spec = workflow(
        "import",
        [
            step("fetch", fetch),
            step("split-a", split(0)).after("fetch"),
            step("split-b", split(1)).after("fetch"),
            step("split-c", split(2)).after("fetch"),
            step("merge", merge)
                .after("split-a")
                .after("split-b")
                .after("split-c"),
        ],
    );
    let run = client.workflows().submit(&spec)?;
    let done = wait_for_workflow(&client, run.id, DEFAULT_TIMEOUT)?;
    assert_eq!(done.state, WorkflowState::Succeeded);
    assert!(done
        .steps
        .values()
        .all(|s| s.state == StepState::Succeeded && s.job.is_some()));

    // Every split got the fetched values, with their JSON types kept.
    let splits = worker.recorder().payloads("split");
    assert_eq!(splits.len(), 3);
    for payload in &splits {
        assert_eq!(payload["source"], "https://example.com/export.csv");
        assert_eq!(payload["rows"], 3);
    }
    let merged = worker.recorder().payloads("merge");
    assert_eq!(
        merged,
        [json!({
            "parts": [0, 1, 2],
            "summary": "merged 3 rows from https://example.com/export.csv",
        })]
    );

    // Fan-out after fetch, fan-in after every split.
    let order: Vec<String> = worker
        .recorder()
        .executions()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(order.first().map(String::as_str), Some("fetch"));
    assert_eq!(order.last().map(String::as_str), Some("merge"));
    assert_eq!(order.len(), 5);

    let finished = events.wait_for_event(DEFAULT_TIMEOUT, |e| {
        e.type_name() == "workflow.finished" && e.kind.workflow() == Some(run.id)
    })?;
    assert!(matches!(
        finished.kind,
        EventKind::WorkflowFinished {
            state: WorkflowState::Succeeded,
            ..
        }
    ));
    let merge_job = done.steps["merge"].job.expect("merge was dispatched");
    let merge_job = client.jobs().get(merge_job)?;
    assert_eq!(merge_job.result, Some(merged[0].clone()));
    Ok(())
}

/// `prepare` -> `explode` (always fails) -> {`after-explode` (needs success),
/// `cleanup` (runs always)}, with `policy` on `explode`.
fn failing_workflow(name: &str, policy: StepFailurePolicy) -> WorkflowSpec {
    workflow(
        name,
        [
            step("prepare", etl("step")),
            step("explode", etl("explode").max_attempts(1))
                .after("prepare")
                .on_failure(policy),
            step("after-explode", etl("step")).after("explode"),
            step("cleanup", etl("step"))
                .after("explode")
                .when(Condition::Always),
        ],
    )
}

#[test]
fn a_failing_step_under_each_failure_policy() -> Result<()> {
    let server = TestServer::start()?;
    let worker = server
        .worker()
        .queue(QUEUE)
        .recording("step")
        .failing("explode")
        .start()?;
    let client = server.client();

    let fail = client.workflows().submit(&failing_workflow(
        "fail-fast",
        StepFailurePolicy::FailWorkflow,
    ))?;
    let go_on = client
        .workflows()
        .submit(&failing_workflow("go-on", StepFailurePolicy::Continue))?;
    let skip = client.workflows().submit(&failing_workflow(
        "skip-rest",
        StepFailurePolicy::SkipDependents,
    ))?;

    // fail_workflow: the run fails and nothing downstream runs.
    let run = wait_for_workflow(&client, fail.id, DEFAULT_TIMEOUT)?;
    assert_eq!(run.state, WorkflowState::Failed);
    assert_eq!(
        step_states(&run),
        [
            ("after-explode".to_owned(), StepState::Cancelled),
            ("cleanup".to_owned(), StepState::Cancelled),
            ("explode".to_owned(), StepState::Failed),
            ("prepare".to_owned(), StepState::Succeeded),
        ]
    );

    // continue: conditions decide; the run still succeeds.
    let run = wait_for_workflow(&client, go_on.id, DEFAULT_TIMEOUT)?;
    assert_eq!(run.state, WorkflowState::Succeeded);
    assert_eq!(
        step_states(&run),
        [
            ("after-explode".to_owned(), StepState::Skipped),
            ("cleanup".to_owned(), StepState::Succeeded),
            ("explode".to_owned(), StepState::Failed),
            ("prepare".to_owned(), StepState::Succeeded),
        ]
    );

    // skip_dependents: everything downstream is skipped, even `always`.
    let run = wait_for_workflow(&client, skip.id, DEFAULT_TIMEOUT)?;
    assert_eq!(run.state, WorkflowState::Succeeded);
    assert_eq!(
        step_states(&run),
        [
            ("after-explode".to_owned(), StepState::Skipped),
            ("cleanup".to_owned(), StepState::Skipped),
            ("explode".to_owned(), StepState::Failed),
            ("prepare".to_owned(), StepState::Succeeded),
        ]
    );

    // Three prepares and one cleanup ran; each explosion ran once.
    assert_eq!(worker.recorder().of_kind("step").len(), 4);
    assert_eq!(worker.recorder().of_kind("explode").len(), 3);
    let dead = run.steps["explode"].job.expect("explode was dispatched");
    assert_eq!(client.jobs().get(dead)?.state, JobState::Dead);
    Ok(())
}

#[test]
fn cancelling_a_workflow_stops_its_running_step_through_the_worker() -> Result<()> {
    let server = TestServer::start()?;
    let gate = Gate::new();
    let worker = server
        .worker()
        .queue(QUEUE)
        .gated("crunch", &gate)
        .recording("report")
        .start()?;
    let client = server.client();
    let spec = workflow(
        "nightly",
        [
            step("crunch", etl("crunch")),
            step("report", etl("report")).after("crunch"),
        ],
    );
    let run = client.workflows().submit(&spec)?;
    let crunch = wait_for_workflow_step(&client, run.id, "crunch")?;
    worker.recorder().wait_for_start(crunch, DEFAULT_TIMEOUT)?;

    client.workflows().cancel(run.id)?;
    let cancelled = wait_for_workflow(&client, run.id, DEFAULT_TIMEOUT)?;
    assert_eq!(cancelled.state, WorkflowState::Cancelled);
    // The step's job was asked to stop and its handler saw it.
    wait_for_state(&client, crunch, JobState::Cancelled, DEFAULT_TIMEOUT)?;
    worker.recorder().wait_until(DEFAULT_TIMEOUT, |r| {
        r.of_job(crunch)
            .iter()
            .any(|e| e.outcome == Recorded::Cancelled)
    })?;
    assert!(worker.recorder().of_kind("report").is_empty());
    assert_ne!(cancelled.steps["report"].state, StepState::Succeeded);
    Ok(())
}

/// Waits until step `name` of run `id` has a job, and returns it.
fn wait_for_workflow_step(
    client: &Client,
    id: belay_proto::WorkflowId,
    name: &str,
) -> Result<JobId> {
    eventually_some(DEFAULT_TIMEOUT, "the step to be dispatched", || {
        client
            .workflows()
            .get(id)
            .ok()
            .and_then(|run| run.steps.get(name).and_then(|s| s.job))
    })
}

#[test]
fn conditions_route_around_a_step_whose_template_cannot_be_filled() -> Result<()> {
    let server = TestServer::start()?;
    let worker = server
        .worker()
        .queue(QUEUE)
        .recording("extract")
        .recording("compensate")
        .recording("report")
        .recording("load")
        .start()?;
    let client = server.client();
    let spec = workflow(
        "partial",
        [
            step("extract", etl("extract").payload(json!({"rows": 10}))),
            // The extract result has no `/missing`: the step fails when it
            // is dispatched, before any job exists.
            step(
                "load",
                etl("load").payload(json!({"rows": placeholder("extract", "/missing")})),
            )
            .after("extract")
            .on_failure(StepFailurePolicy::Continue),
            step("compensate", etl("compensate"))
                .after("load")
                .when(Condition::AnyFailed),
            step(
                "report",
                etl("report").payload(json!({"rows": placeholder("extract", ".rows")})),
            )
            .after("extract")
            .after("load")
            .when(Condition::AnySucceeded),
        ],
    );
    let run = client.workflows().submit(&spec)?;
    let done = wait_for_workflow(&client, run.id, DEFAULT_TIMEOUT)?;
    assert_eq!(done.state, WorkflowState::Succeeded);
    assert_eq!(done.steps["load"].state, StepState::Failed);
    assert_eq!(done.steps["load"].job, None, "no job was created for it");
    assert_eq!(done.steps["compensate"].state, StepState::Succeeded);
    assert_eq!(done.steps["report"].state, StepState::Succeeded);
    assert_eq!(worker.recorder().payloads("report"), [json!({"rows": 10})]);
    assert!(worker.recorder().of_kind("load").is_empty());

    // The failure names the placeholder that could not be filled.
    let details = belay_testkit::raw::request(
        "GET",
        &format!("{}/v1/workflows/{}?details=true", server.url(), run.id),
        &[],
        b"",
    )?
    .json()?;
    let error = details["errors"]["load"].as_str().unwrap_or_default();
    assert!(
        error.contains("{{steps.extract.result/missing}}"),
        "{error}"
    );
    Ok(())
}

#[test]
fn a_workflow_carries_on_across_a_server_restart() -> Result<()> {
    let mut server = TestServer::builder()
        .storage(Storage::Disk {
            sync: belay_store::SyncPolicy::Interval(std::time::Duration::from_millis(10)),
        })
        .start()?;
    let gate = Gate::new();
    let worker = server
        .worker()
        .queue(QUEUE)
        .recording("first")
        .gated("second", &gate)
        .recording("third")
        .start()?;
    let client = server.client();
    let spec = workflow(
        "long-haul",
        [
            step("first", etl("first").payload(json!({"batch": 7}))),
            step(
                "second",
                etl("second").payload(json!({"batch": placeholder("first", ".batch")})),
            )
            .after("first"),
            step("third", etl("third")).after("second"),
        ],
    );
    let run = client.workflows().submit(&spec)?;
    let second = wait_for_workflow_step(&client, run.id, "second")?;
    worker.recorder().wait_for_start(second, DEFAULT_TIMEOUT)?;

    // The server goes down while the second step works; its worker keeps
    // its lease and reports once the server is back.
    server.restart()?;
    let client = server.client();
    assert_eq!(
        client.workflows().get(run.id)?.steps["second"].state,
        StepState::Running
    );
    gate.open();
    let done = wait_for_workflow(&client, run.id, DEFAULT_TIMEOUT)?;
    assert_eq!(done.state, WorkflowState::Succeeded);
    assert_eq!(worker.recorder().payloads("second"), [json!({"batch": 7})]);
    assert_eq!(worker.recorder().of_kind("third").len(), 1);
    assert_eq!(client.jobs().get(second)?.attempt, 1);
    Ok(())
}
