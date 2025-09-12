//! Workflow runs: DAG shapes, failure policies, conditions, skip
//! propagation, payload templating, cancellation, listing and retention.

mod support;

use std::time::Duration;

use belay_common::Clock;
use belay_json::{json, Value};
use belay_proto::api::SortOrder;
use belay_proto::event::EventKind;
use belay_proto::workflow::{Condition, StepFailurePolicy, StepState, WorkflowState};
use belay_proto::{Job, JobSpec, JobState, UniqueScope, WorkflowId};
use belay_scheduler::{step_key, DispatchError, Dispatcher as _, WorkflowFilter};
use belay_store::{KeyRange, Kv};

use support::{step, workflow, Flow};

fn state_of(f: &Flow, run: WorkflowId, name: &str) -> StepState {
    f.get(run).steps[name].state
}

fn diamond() -> belay_proto::WorkflowSpec {
    workflow(
        "diamond",
        vec![
            step("fetch", &[]),
            step("left", &["fetch"]),
            step("right", &["fetch"]),
            step("join", &["left", "right"]),
        ],
    )
}

#[test]
fn submit_rejects_bad_graphs_and_placeholders() {
    let f = Flow::new();
    let cyclic = workflow("loop", vec![step("a", &["b"]), step("b", &["a"])]);
    assert_eq!(
        f.engine.submit(cyclic).unwrap_err().to_string(),
        "$.steps: dependency cycle: a -> b -> a (each step depends on the next)"
    );
    let mut bad = workflow("bad", vec![step("a", &[]), step("b", &["a"])]);
    bad.steps[1].job.payload =
        json!({"url": "{{steps.a.results}}", "n": ["{{steps.a.result.x..y}}"]});
    let problems: Vec<String> = f
        .engine
        .submit(bad)
        .unwrap_err()
        .violations()
        .unwrap()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        problems,
        [
            r#"$.steps[1].job.payload: invalid placeholder {{steps.a.results}}: unexpected "s" after "result" (use /json/pointer or .dotted.keys) (at /url)"#,
            "$.steps[1].job.payload: invalid placeholder {{steps.a.result.x..y}}: empty key in dotted path (at /n/0)",
        ]
    );
    let mut orphan = workflow("orphan", vec![step("a", &[]), step("b", &[])]);
    orphan.steps[1].job.payload = json!("{{steps.a.result}}");
    assert!(
        f.engine.submit(orphan).is_err(),
        "a is not an ancestor of b"
    );
    assert!(f.engine.is_empty());
    assert_eq!(f.queue.job_count(), 0);
}

#[test]
fn a_diamond_joins_after_both_branches() {
    let f = Flow::new();
    let run = f.submit(diamond());
    assert_eq!(run.state, WorkflowState::Running);
    assert_eq!(run.steps["fetch"].state, StepState::Running);
    assert_eq!(f.queue.job_count(), 1);

    let fetch = f.job(run.id, "fetch");
    let link = fetch.spec.workflow.clone().unwrap();
    assert_eq!((link.id, link.step.as_str()), (run.id, "fetch"));
    let unique = fetch.spec.unique.unwrap();
    assert_eq!(unique.key, step_key(run.id, "fetch"));
    assert_eq!(unique.scope, UniqueScope::For(Duration::from_secs(86_400)));

    f.succeed(run.id, "fetch", Value::Null);
    assert!(f.dispatched(run.id, "left") && f.dispatched(run.id, "right"));
    let after_left = f.succeed(run.id, "left", Value::Null);
    assert_eq!(
        after_left.steps["join"].state,
        StepState::Pending,
        "waits for right"
    );
    assert!(!f.dispatched(run.id, "join"));
    f.succeed(run.id, "right", Value::Null);
    let done = f.succeed(run.id, "join", Value::Null);
    assert_eq!(done.state, WorkflowState::Succeeded);
    assert_eq!(done.finished_at, Some(f.clock.now()));
    assert_eq!(f.engine.running(), 0);

    let events = f.events();
    assert_eq!(events.first(), Some(&"workflow.started"));
    assert_eq!(events.last(), Some(&"workflow.finished"));
    // Started, then running and succeeded for each of the four steps.
    assert_eq!(events.len(), 1 + 8 + 1);
}

#[test]
fn fan_out_starts_every_shard_and_fan_in_waits_for_all() {
    let f = Flow::new();
    let mut steps = vec![step("split", &[])];
    let shards: Vec<String> = (0..5).map(|i| format!("shard-{i}")).collect();
    for s in &shards {
        steps.push(step(s, &["split"]));
    }
    let names: Vec<&str> = shards.iter().map(String::as_str).collect();
    steps.push(step("merge", &names));
    let run = f.submit(workflow("fan", steps));
    f.succeed(run.id, "split", Value::Null);
    assert_eq!(f.queue.job_count(), 6, "every shard starts at once");
    for (i, s) in shards.iter().rev().enumerate() {
        assert!(!f.dispatched(run.id, "merge"), "merge before {i} shards");
        f.succeed(run.id, s, json!(i));
    }
    assert!(f.dispatched(run.id, "merge"));
    let done = f.succeed(run.id, "merge", Value::Null);
    assert_eq!(done.state, WorkflowState::Succeeded);
}

#[test]
fn fail_workflow_cancels_everything_still_open() {
    let f = Flow::new();
    let run = f.submit(workflow(
        "etl",
        vec![step("a", &[]), step("b", &[]), step("c", &["a"])],
    ));
    let b = f.job(run.id, "b").id;
    let failed = f.fail(run.id, "a", "connection refused");
    assert_eq!(failed.state, WorkflowState::Failed);
    assert_eq!(failed.steps["a"].state, StepState::Failed);
    assert_eq!(failed.steps["b"].state, StepState::Cancelled);
    assert_eq!(failed.steps["c"].state, StepState::Cancelled);
    assert_eq!(f.queue.job_state(&b), Some(JobState::Cancelled));
    assert!(!f.dispatched(run.id, "c"));
    let details = f.engine.details(run.id).unwrap().unwrap();
    let a = f.job(run.id, "a").id;
    assert_eq!(
        details.error("a"),
        Some(format!("job {a} is dead: connection refused").as_str())
    );
    // b's cancellation arriving later changes nothing.
    let cancelled = f.queue.get(&b).unwrap();
    assert_eq!(f.engine.on_job_finished(&cancelled).unwrap(), None);
}

#[test]
fn continue_lets_downstream_conditions_decide() {
    let f = Flow::new();
    let run = f.submit(workflow(
        "etl",
        vec![
            step("load", &[]).on_failure(StepFailurePolicy::Continue),
            step("report", &["load"]),
            step("cleanup", &["load"]).when(Condition::AnyFailed),
        ],
    ));
    let after = f.fail(run.id, "load", "disk full");
    assert_eq!(after.steps["report"].state, StepState::Skipped);
    assert_eq!(after.steps["cleanup"].state, StepState::Running);
    let done = f.succeed(run.id, "cleanup", Value::Null);
    // A failure under `continue` does not fail the run.
    assert_eq!(done.state, WorkflowState::Succeeded);
}

#[test]
fn skips_propagate_through_default_conditions() {
    let f = Flow::new();
    let run = f.submit(workflow(
        "etl",
        vec![
            step("a", &[]),
            step("on-error", &["a"]).when(Condition::AnyFailed),
            step("after-error", &["on-error"]),
            step("notify", &["on-error"]).when(Condition::Always),
        ],
    ));
    let after = f.succeed(run.id, "a", Value::Null);
    assert_eq!(after.steps["on-error"].state, StepState::Skipped);
    assert_eq!(after.steps["after-error"].state, StepState::Skipped);
    assert_eq!(after.steps["notify"].state, StepState::Running);
    assert_eq!(
        f.succeed(run.id, "notify", Value::Null).state,
        WorkflowState::Succeeded
    );
}

#[test]
fn any_succeeded_needs_just_one_parent() {
    let f = Flow::new();
    let run = f.submit(workflow(
        "mirror",
        vec![
            step("primary", &[]).on_failure(StepFailurePolicy::Continue),
            step("backup", &[]).on_failure(StepFailurePolicy::Continue),
            step("publish", &["primary", "backup"]).when(Condition::AnySucceeded),
        ],
    ));
    f.fail(run.id, "primary", "timeout");
    assert!(!f.dispatched(run.id, "publish"), "backup has not finished");
    let after = f.succeed(run.id, "backup", json!({"ok": true}));
    assert_eq!(after.steps["publish"].state, StepState::Running);
}

#[test]
fn placeholders_pass_results_downstream() {
    let f = Flow::new();
    let mut spec = workflow(
        "crawl",
        vec![
            step("fetch", &[]),
            step("process", &["fetch"]),
            step("tidy", &["process"]),
        ],
    );
    spec.steps[1].job.payload = json!({
        "url": "{{steps.fetch.result.url}}",
        "first": "{{ steps.fetch.result/items/0 }}",
        "greeting": "Hello {{ user }}",
        "fixed": 7
    });
    let run = f.submit(spec);
    f.succeed(
        run.id,
        "fetch",
        json!({"url": "http://example.com/feed", "items": [{"id": 1}, {"id": 2}], "count": 2}),
    );
    assert_eq!(
        f.job(run.id, "process").spec.payload,
        json!({
            "url": "http://example.com/feed",
            "first": {"id": 1},
            "greeting": "Hello {{ user }}",
            "fixed": 7
        })
    );
    // Only results that some placeholder reads are kept.
    let kept: Vec<String> =
        f.kv.scan_keys(KeyRange::prefix("wfr/"))
            .unwrap()
            .into_iter()
            .map(|k| String::from_utf8(k).unwrap())
            .collect();
    assert_eq!(kept, [format!("wfr/{}/fetch", run.id)]);
    f.succeed(run.id, "process", json!("not kept"));
    assert_eq!(f.kv.count(KeyRange::prefix("wfr/")).unwrap(), 1);
}

#[test]
fn a_missing_placeholder_value_fails_the_step_with_a_clear_error() {
    let f = Flow::new();
    let mut spec = workflow(
        "crawl",
        vec![step("fetch", &[]), step("process", &["fetch"])],
    );
    spec.steps[1].job.payload = json!({"url": "{{steps.fetch.result.url}}"});
    let run = f.submit(spec);
    let after = f.succeed(run.id, "fetch", json!({"status": 404}));
    assert_eq!(after.steps["process"].state, StepState::Failed);
    assert_eq!(after.state, WorkflowState::Failed);
    assert!(!f.dispatched(run.id, "process"));
    assert_eq!(
        f.engine.details(run.id).unwrap().unwrap().error("process"),
        Some(
            r#"placeholder {{steps.fetch.result.url}}: the result of step "fetch" has no value at /url"#
        )
    );
}

#[test]
fn a_placeholder_on_a_failed_step_fails_its_reader() {
    let f = Flow::new();
    let mut spec = workflow(
        "crawl",
        vec![
            step("fetch", &[]).on_failure(StepFailurePolicy::Continue),
            step("report", &["fetch"]).when(Condition::Always),
        ],
    );
    spec.steps[1].job.payload = json!("status: {{steps.fetch.result.status}}");
    let run = f.submit(spec);
    let after = f.fail(run.id, "fetch", "boom");
    assert_eq!(after.steps["report"].state, StepState::Failed);
    assert_eq!(
        f.engine.details(run.id).unwrap().unwrap().error("report"),
        Some(
            r#"placeholder {{steps.fetch.result.status}}: step "fetch" is failed, so it has no result"#
        )
    );
}

#[test]
fn cancel_stops_running_steps_and_marks_pending_ones() {
    let f = Flow::new();
    let run = f.submit(workflow("etl", vec![step("a", &[]), step("b", &["a"])]));
    let a = f.job(run.id, "a").id;
    f.queue.start(&a);
    f.events();
    let cancelled = f.engine.cancel(run.id).unwrap();
    assert_eq!(cancelled.state, WorkflowState::Cancelled);
    assert_eq!(cancelled.steps["a"].state, StepState::Cancelled);
    assert_eq!(cancelled.steps["b"].state, StepState::Cancelled);
    assert_eq!(f.queue.cancel_requests(), [a]);
    assert_eq!(
        f.events(),
        ["workflow.step", "workflow.step", "workflow.finished"]
    );
    // Cancelling again, or the worker acknowledging, changes nothing.
    assert_eq!(f.engine.cancel(run.id).unwrap(), cancelled);
    let acknowledged = f.queue.finish(&a, JobState::Cancelled, None, None).unwrap();
    assert_eq!(f.engine.on_job_finished(&acknowledged).unwrap(), None);
    assert!(f
        .engine
        .cancel(WorkflowId::generate())
        .unwrap_err()
        .is_not_found());
}

#[test]
fn on_job_finished_ignores_repeats_strangers_and_unfinished_jobs() {
    let f = Flow::new();
    let run = f.submit(workflow("etl", vec![step("a", &[]), step("b", &["a"])]));
    let job = f
        .queue
        .succeed(&f.job(run.id, "a").id, Value::Null)
        .unwrap();
    assert!(f.engine.on_job_finished(&job).unwrap().is_some());
    assert_eq!(f.engine.on_job_finished(&job).unwrap(), None, "repeat");

    let plain = f.queue.dispatch(&JobSpec::new("etl", "x")).unwrap().id;
    let plain = f.queue.succeed(&plain, Value::Null).unwrap();
    assert_eq!(
        f.engine.on_job_finished(&plain).unwrap(),
        None,
        "no workflow"
    );

    let running = f.queue.start(&f.job(run.id, "b").id).unwrap();
    assert_eq!(
        f.engine.on_job_finished(&running).unwrap(),
        None,
        "not finished"
    );

    let mut stranger: Job = job.clone();
    stranger.spec.workflow.as_mut().unwrap().id = WorkflowId::generate();
    assert_eq!(
        f.engine.on_job_finished(&stranger).unwrap(),
        None,
        "unknown run"
    );
}

#[test]
fn a_rejected_dispatch_fails_the_step() {
    let f = Flow::new();
    f.queue.fail_next(DispatchError::Rejected(
        "queue etl does not accept jobs".into(),
    ));
    let run = f.submit(workflow("etl", vec![step("a", &[]), step("b", &["a"])]));
    assert_eq!(run.state, WorkflowState::Failed);
    assert_eq!(
        f.engine.details(run.id).unwrap().unwrap().error("a"),
        Some("dispatch rejected: queue etl does not accept jobs")
    );
}

#[test]
fn an_unavailable_dispatcher_defers_the_step_until_recovery() {
    let f = Flow::new();
    f.queue
        .fail_next(DispatchError::Unavailable("queue restarting".into()));
    let run = f.submit(workflow("etl", vec![step("a", &[]), step("b", &["a"])]));
    assert_eq!(run.state, WorkflowState::Running);
    assert_eq!(run.steps["a"].state, StepState::Pending);
    assert_eq!(f.queue.job_count(), 0);
    assert_eq!(f.engine.recover().unwrap(), 1);
    assert_eq!(state_of(&f, run.id, "a"), StepState::Running);
    assert_eq!(f.engine.recover().unwrap(), 0, "nothing left to do");
}

#[test]
fn a_step_with_its_own_unique_key_keeps_it() {
    let f = Flow::new();
    let mut spec = workflow("etl", vec![step("a", &[])]);
    spec.steps[0].job = spec.steps[0]
        .job
        .clone()
        .unique("tenant-7:export", UniqueScope::UntilFinished);
    let run = f.submit(spec);
    assert_eq!(
        f.job(run.id, "a").spec.unique.unwrap().key,
        "tenant-7:export"
    );
}
