//! A job's normal life through a running worker: success, failure,
//! retries, unknown kinds, panics, progress, middleware, and concurrency
//! limits.

mod support;

use std::sync::{Arc, Mutex};

use belay_json::{json, Value};
use belay_proto::limits::MAX_RESULT_BYTES;
use belay_worker::builtin::{EchoHandler, FailHandler};
use belay_worker::middleware::{middleware_fn, Next, TimingMiddleware};
use belay_worker::testing::{JobOutcome, MemorySource};
use belay_worker::{
    failure_kind, handler_fn, HandlerError, JobContext, KindOptions, Outcome, Registry,
};

use support::{config, eventually, spec, start, Concurrency, Gate, PATIENCE};

fn failure(source: &MemorySource, id: &belay_proto::JobId) -> belay_proto::api::FailRequest {
    source
        .outcome(id)
        .and_then(|o| o.failure().cloned())
        .expect("job failed")
}

#[test]
fn completes_jobs_with_their_results() {
    let source = Arc::new(MemorySource::new());
    let ids: Vec<_> = (0..3)
        .map(|i| source.push(spec("echo").payload(json!({"n": i}))))
        .collect();
    let worker = start(
        &source,
        Registry::new().register("echo", EchoHandler),
        config(2),
    );

    assert!(source.wait_until(PATIENCE, |v| v.completed() == 3));
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(
            source.outcome(id),
            Some(JobOutcome::Completed(Some(json!({"n": i}))))
        );
    }
    let report = worker.stop();
    assert_eq!(
        (
            report.stats.leased,
            report.stats.succeeded,
            report.stats.failed
        ),
        (3, 3, 0)
    );
    assert!(report.drained);
    assert_eq!(report.stats.running, 0);
}

#[test]
fn handler_errors_are_reported_with_kind_and_retryability() {
    let source = Arc::new(MemorySource::new());
    let id = source.push(spec("validate"));
    let registry = Registry::new().register(
        "validate",
        handler_fn(|_: &mut JobContext| {
            Err(HandlerError::fatal("unknown template \"welcom\"").with_kind("bad input"))
        }),
    );
    let worker = start(&source, registry, config(1));

    assert!(source.wait_until(PATIENCE, |v| v.failed() == 1));
    let req = failure(&source, &id);
    assert_eq!(req.error, "unknown template \"welcom\"");
    assert_eq!(
        req.kind.as_deref(),
        Some("bad_input"),
        "kinds are cleaned for the wire"
    );
    assert!(!req.retryable);
    let stats = worker.stop().stats;
    assert_eq!((stats.failed, stats.retried), (1, 0));
    assert_eq!(
        stats.last_error.as_deref(),
        Some("unknown template \"welcom\"")
    );
}

#[test]
fn retryable_failures_run_again_until_they_succeed() {
    let source = Arc::new(MemorySource::new());
    let id = source.push(spec("flaky").max_attempts(5));
    let registry =
        Registry::new().register("flaky", FailHandler::new("try again").succeed_on_attempt(3));
    let worker = start(&source, registry, config(1));

    assert!(source.wait_until(PATIENCE, |v| v.completed() == 1));
    let history = source.view(|v| v.outcomes_for(&id).into_iter().cloned().collect::<Vec<_>>());
    assert_eq!(history.len(), 3);
    assert!(history[..2]
        .iter()
        .all(|o| o.failure().is_some_and(|f| f.retryable)));
    assert_eq!(
        history[2],
        JobOutcome::Completed(Some(json!({"attempt": 3})))
    );
    let stats = worker.stop().stats;
    assert_eq!(
        (stats.leased, stats.failed, stats.retried, stats.succeeded),
        (3, 2, 2, 1)
    );
}

#[test]
fn unknown_kinds_fail_permanently() {
    let source = Arc::new(MemorySource::new());
    let id = source.push(spec("mystery"));
    let worker = start(
        &source,
        Registry::new().register("echo", EchoHandler),
        config(1),
    );

    assert!(source.wait_until(PATIENCE, |v| v.failed() == 1));
    let req = failure(&source, &id);
    assert_eq!(req.error, r#"no handler registered for kind "mystery""#);
    assert_eq!(req.kind.as_deref(), Some(failure_kind::UNKNOWN_KIND));
    assert!(!req.retryable);
    assert_eq!(worker.stop().stats.failed, 1);
}

#[test]
fn panics_fail_the_attempt_retryably_and_the_executor_survives() {
    let source = Arc::new(MemorySource::new());
    source.set_requeue(false);
    let boom = source.push(spec("boom"));
    let registry = Registry::new()
        .register(
            "boom",
            FailHandler::new("index out of bounds at step 2").panicking(),
        )
        .register("echo", EchoHandler);
    let worker = start(&source, registry, config(1));

    assert!(source.wait_until(PATIENCE, |v| v.failed() == 1));
    let req = failure(&source, &boom);
    assert_eq!(req.error, "handler panicked: index out of bounds at step 2");
    assert_eq!(req.kind.as_deref(), Some(failure_kind::PANIC));
    assert!(req.retryable);

    // The only executor thread caught the panic and keeps working.
    let after = source.push(spec("echo").payload(json!("still here")));
    assert!(source.wait_until(PATIENCE, |v| v.outcome(&after).is_some()));
    assert_eq!(
        source.outcome(&after),
        Some(JobOutcome::Completed(Some(json!("still here"))))
    );
    let stats = worker.stop().stats;
    assert_eq!(
        (stats.panicked, stats.failed, stats.retried, stats.succeeded),
        (1, 1, 1, 1)
    );
}

#[test]
fn oversized_results_fail_instead_of_being_rejected_by_the_source() {
    let source = Arc::new(MemorySource::new());
    let id = source.push(spec("big"));
    let registry = Registry::new().register(
        "big",
        handler_fn(|_: &mut JobContext| Ok(Outcome::with("x".repeat(MAX_RESULT_BYTES)))),
    );
    let worker = start(&source, registry, config(1));

    assert!(source.wait_until(PATIENCE, |v| v.failed() == 1));
    let req = failure(&source, &id);
    assert_eq!(req.kind.as_deref(), Some(failure_kind::RESULT_TOO_LARGE));
    assert_eq!(
        req.error,
        format!(
            "result is {} bytes as JSON; the limit is {MAX_RESULT_BYTES}",
            MAX_RESULT_BYTES + 2
        )
    );
    assert!(!req.retryable);
    drop(worker);
}

#[test]
fn undecodable_payloads_fail_permanently() {
    let source = Arc::new(MemorySource::new());
    let id = source.push(spec("count").payload(json!("seven")));
    let registry = Registry::new().register(
        "count",
        handler_fn(|ctx: &mut JobContext| {
            let n: u32 = ctx.payload_as()?;
            Ok(Outcome::with(n))
        }),
    );
    let worker = start(&source, registry, config(1));

    assert!(source.wait_until(PATIENCE, |v| v.failed() == 1));
    let req = failure(&source, &id);
    assert_eq!(req.kind.as_deref(), Some(failure_kind::INVALID_PAYLOAD));
    assert!(
        req.error
            .starts_with("invalid payload: $: expected integer"),
        "{}",
        req.error
    );
    assert!(!req.retryable);
    drop(worker);
}

#[test]
fn explicit_null_results_are_kept_apart_from_no_result() {
    let source = Arc::new(MemorySource::new());
    let null = source.push(spec("null"));
    let nothing = source.push(spec("nothing"));
    let registry = Registry::new()
        .register(
            "null",
            handler_fn(|_: &mut JobContext| Ok(Outcome::with(Value::Null))),
        )
        .register(
            "nothing",
            handler_fn(|_: &mut JobContext| Ok(Outcome::done())),
        );
    let worker = start(&source, registry, config(2));

    assert!(source.wait_until(PATIENCE, |v| v.completed() == 2));
    assert_eq!(
        source.outcome(&null),
        Some(JobOutcome::Completed(Some(Value::Null)))
    );
    assert_eq!(source.outcome(&nothing), Some(JobOutcome::Completed(None)));
    drop(worker);
}

#[test]
fn progress_is_delivered_once_with_the_next_heartbeat() {
    let source = Arc::new(MemorySource::new());
    let id = source.push(spec("report"));
    let gate = Gate::new();
    let g = Arc::clone(&gate);
    let registry = Registry::new().register(
        "report",
        handler_fn(move |ctx: &mut JobContext| {
            ctx.progress(50.0, "half way");
            g.pass();
            Ok(Outcome::done())
        }),
    );
    let worker = start(&source, registry, config(1));

    assert!(source.wait_until(PATIENCE, |v| !v.progress_for(&id).is_empty()));
    // A few more heartbeats go out while the handler is held; none of
    // them repeats the progress.
    let beats = source.view(|v| v.heartbeats_for(&id));
    assert!(source.wait_until(PATIENCE, |v| v.heartbeats_for(&id) >= beats + 2));
    gate.open();
    assert!(source.wait_until(PATIENCE, |v| v.completed() == 1));

    let delivered = source.view(|v| v.progress_for(&id).into_iter().cloned().collect::<Vec<_>>());
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].percent, Some(50.0));
    assert_eq!(delivered[0].message.as_deref(), Some("half way"));
    drop(worker);
}

#[test]
fn middleware_wraps_handlers_in_registration_order() {
    let source = Arc::new(MemorySource::new());
    source.push(spec("work"));
    let trail = Arc::new(Mutex::new(Vec::new()));
    let layer = |name: &'static str| {
        let trail = Arc::clone(&trail);
        middleware_fn(move |ctx: &mut JobContext, next: Next<'_>| {
            trail.lock().unwrap().push(format!("{name} in"));
            let result = next.run(ctx);
            trail.lock().unwrap().push(format!("{name} out"));
            result
        })
    };
    let handler_trail = Arc::clone(&trail);
    let timing = TimingMiddleware::new();
    let timings = timing.timings();
    let registry = Registry::new()
        .register(
            "work",
            handler_fn(move |_: &mut JobContext| {
                handler_trail.lock().unwrap().push("handler".to_owned());
                Ok(Outcome::done())
            }),
        )
        .wrap(layer("auth"))
        .wrap(timing)
        .wrap(layer("tracing"));
    let worker = start(&source, registry, config(1));

    assert!(source.wait_until(PATIENCE, |v| v.completed() == 1));
    assert_eq!(
        *trail.lock().unwrap(),
        [
            "auth in",
            "tracing in",
            "handler",
            "tracing out",
            "auth out"
        ]
    );
    assert_eq!(timings.get("work").map(|t| t.runs), Some(1));
    drop(worker);
}

#[test]
fn concurrency_bounds_handlers_and_leases() {
    let source = Arc::new(MemorySource::new());
    for _ in 0..9 {
        source.push(spec("gated"));
    }
    let gate = Gate::new();
    let seen = Concurrency::new();
    let (g, s) = (Arc::clone(&gate), Arc::clone(&seen));
    let registry = Registry::new().register(
        "gated",
        handler_fn(move |_: &mut JobContext| {
            s.enter();
            g.pass();
            s.leave();
            Ok(Outcome::done())
        }),
    );
    let worker = start(&source, registry, config(3));

    assert!(eventually(|| gate.entered() == 3));
    let stats = worker.stats();
    assert_eq!(
        (stats.running, stats.busy_slots, stats.idle_slots()),
        (3, 3, 0)
    );
    assert_eq!(source.view(|v| v.leased()), 3, "no more leases than slots");

    gate.open();
    assert!(source.wait_until(PATIENCE, |v| v.completed() == 9));
    assert_eq!(seen.max(), 3);
    let stats = worker.stop().stats;
    assert_eq!((stats.running, stats.busy_slots), (0, 0));
}

#[test]
fn per_kind_limits_hold_while_other_kinds_run() {
    let source = Arc::new(MemorySource::new());
    let slow: Vec<_> = (0..2).map(|_| source.push(spec("slow"))).collect();
    let fast: Vec<_> = (0..3).map(|_| source.push(spec("fast"))).collect();
    let gate = Gate::new();
    let seen = Concurrency::new();
    let (g, s) = (Arc::clone(&gate), Arc::clone(&seen));
    let registry = Registry::new()
        .register_with(
            "slow",
            handler_fn(move |_: &mut JobContext| {
                s.enter();
                g.pass();
                s.leave();
                Ok(Outcome::done())
            }),
            KindOptions::new().concurrency(1),
        )
        .register("fast", EchoHandler);
    let worker = start(&source, registry, config(4));

    // The first lease takes both slow jobs and two fast ones: one slow job
    // runs, the other waits for the limit, the fast ones finish.
    assert!(source.wait_until(PATIENCE, |v| v.completed() == 2));
    assert!(eventually(|| worker.stats().waiting == 1));
    assert_eq!(gate.entered(), 1);
    // While a job waits for its kind, nothing more is leased.
    assert_eq!(source.view(|v| v.ready()), 1);
    assert!(source.outcome(&fast[2]).is_none());

    gate.open();
    assert!(source.wait_until(PATIENCE, |v| v.completed() == 5));
    assert_eq!(seen.max(), 1);
    for id in slow.iter().chain(&fast) {
        assert!(matches!(source.outcome(id), Some(JobOutcome::Completed(_))));
    }
    drop(worker);
}

#[test]
fn when_every_kind_is_limited_only_runnable_jobs_are_leased() {
    let source = Arc::new(MemorySource::new());
    for _ in 0..3 {
        source.push(spec("serial"));
    }
    let gate = Gate::new();
    let g = Arc::clone(&gate);
    let registry = Registry::new().register_with(
        "serial",
        handler_fn(move |_: &mut JobContext| {
            g.pass();
            Ok(Outcome::done())
        }),
        KindOptions::new().concurrency(1),
    );
    let worker = start(&source, registry, config(4));

    assert!(eventually(|| gate.entered() == 1));
    // Three slots are free, but no job the worker could lease would run.
    assert!(!source.wait_until(std::time::Duration::from_millis(20), |v| v.leased() > 1));
    assert_eq!(source.view(|v| v.ready()), 2);
    assert_eq!(worker.stats().waiting, 0);

    gate.open();
    assert!(source.wait_until(PATIENCE, |v| v.completed() == 3));
    drop(worker);
}

#[test]
fn jobs_from_other_queues_are_left_alone() {
    let source = Arc::new(MemorySource::new());
    let elsewhere = source.push(belay_proto::JobSpec::new("other", "echo"));
    let here = source.push(spec("echo"));
    let worker = start(
        &source,
        Registry::new().register("echo", EchoHandler),
        config(1),
    );

    assert!(source.wait_until(PATIENCE, |v| v.outcome(&here).is_some()));
    assert!(source.outcome(&elsewhere).is_none());
    assert_eq!(source.view(|v| v.ready()), 1);
    drop(worker);
}
