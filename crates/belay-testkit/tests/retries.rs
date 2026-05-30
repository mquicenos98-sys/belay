//! Failed attempts: backoff on the job's policy with a manual clock,
//! exhausting attempts into `dead` and an operator's retry out of it,
//! non-retryable failures, handler-chosen delays, panics, and a worker
//! that stops in the middle of a job.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use belay_common::BackoffPolicy;
use belay_testkit::prelude::*;
use belay_worker::{handler_fn, HandlerError, Outcome};

fn retry_times(events: &EventLog, id: JobId) -> Vec<belay_common::Timestamp> {
    events
        .of_job(id)
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::JobFailed { retry_at, .. } => Some(retry_at),
            _ => None,
        })
        .collect()
}

#[test]
fn failed_attempts_back_off_on_the_job_policy_then_succeed() -> Result<()> {
    let server = TestServer::start()?;
    let events = server.event_log()?;
    let worker = server
        .worker()
        .queue("sync")
        .failing_n_times("pull", 2)
        .start()?;
    let client = server.client();
    let t0 = server.now();
    let policy = BackoffPolicy::exponential(Duration::from_secs(10), 2.0, Duration::from_secs(60));
    let spec = job("sync", "pull")
        .retry(policy)
        .max_attempts(5)
        .payload(json!({"repo": "belay"}));
    let id = client.jobs().enqueue(&spec)?.id;

    let first = wait_for_job(
        &client,
        id,
        "retrying after attempt 1",
        DEFAULT_TIMEOUT,
        |j| j.state == JobState::Retrying && j.attempt == 1,
    )?;
    assert_eq!(first.run_at, t0 + Duration::from_secs(10));
    assert_eq!(first.errors.len(), 1);
    assert_eq!(first.errors[0].attempt, 1);
    assert!(first.errors[0].retryable);
    assert_eq!(first.errors[0].message, "pull fails on purpose (1 of 2)");

    // One second early: the worker asks, and nothing is handed out.
    server.tick(Duration::from_secs(9))?;
    worker.wait_for_empty_leases(2, DEFAULT_TIMEOUT)?;
    assert_eq!(client.jobs().get(id)?.attempt, 1);

    server.tick(Duration::from_secs(1))?;
    let second = wait_for_job(
        &client,
        id,
        "retrying after attempt 2",
        DEFAULT_TIMEOUT,
        |j| j.state == JobState::Retrying && j.attempt == 2,
    )?;
    // The second delay doubles, counted from the second failure.
    assert_eq!(second.run_at, t0 + Duration::from_secs(10 + 20));

    server.tick(Duration::from_secs(20))?;
    let done = wait_for_state(&client, id, JobState::Succeeded, DEFAULT_TIMEOUT)?;
    assert_eq!(done.attempt, 3);
    assert_eq!(done.errors.len(), 2, "the error history is kept");
    assert_eq!(done.result, Some(json!({"repo": "belay"})));
    assert_eq!(worker.recorder().attempts_of(id), [1, 2, 3]);

    events.wait_for_job_event(id, "job.succeeded", DEFAULT_TIMEOUT)?;
    assert_eq!(
        events.types_of_job(id),
        [
            "job.enqueued",
            "job.started",
            "job.failed",
            "job.started",
            "job.failed",
            "job.started",
            "job.succeeded"
        ]
    );
    assert_eq!(
        retry_times(&events, id),
        [t0 + Duration::from_secs(10), t0 + Duration::from_secs(30)]
    );
    Ok(())
}
