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
