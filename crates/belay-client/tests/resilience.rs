//! Retries, error mapping, cluster redirects and long-poll timeouts,
//! observed from the server side by counting attempts.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use belay_client::{Client, Error};
use belay_common::log::CaptureLogger;
use belay_http::{Request, Response, Router, Server, StatusCode};
use belay_json::json;
use belay_proto::api::{
    ApiError, EnqueueResponse, EnqueueResult, EnqueueStatus, ErrorCode, HeartbeatRequest,
    HeartbeatResponse, JobFilter, LeaseRequest, LeaseResponse, Page,
};
use belay_proto::{JobSpec, JobState, LeaseToken, QueueName, ScheduleId, UniqueScope, WorkerId};
use common::{
    fast_config, job_id, sample_job, server_config, t0, unreachable_config, Fake, Hangup,
};

fn unavailable() -> ApiError {
    ApiError::unavailable("node is draining")
}

fn created() -> EnqueueResponse {
    EnqueueResponse {
        results: vec![EnqueueResult {
            id: job_id(1),
            status: EnqueueStatus::Created,
            state: JobState::Ready,
        }],
    }
}

fn lease_request() -> LeaseRequest {
    LeaseRequest::new(
        WorkerId::from_static("w1"),
        vec![QueueName::from_static("emails")],
    )
}

// ---- retries ------------------------------------------------------------

#[test]
fn reads_are_retried_through_a_brief_outage() {
    let fake = Fake::start();
    fake.reply_error(unavailable());
    fake.reply_status(502);
    fake.reply_json(200, &sample_job(1));

    let job = fake
        .client()
        .jobs()
        .get(job_id(1))
        .expect("third attempt works");

    assert_eq!(job.id, job_id(1));
    assert_eq!(fake.count(), 3);
}

#[test]
fn the_retry_budget_ends_with_the_last_error() {
    let fake = Fake::start();
    for _ in 0..3 {
        fake.reply_error(unavailable());
    }
    let client = fake.client_with(|c| c.retries(2));

    let err = client.jobs().get(job_id(1)).unwrap_err();

    assert_eq!(err.api_error(), Some(&unavailable()));
    assert!(err.is_retryable(), "the caller may still try later");
    assert_eq!(fake.count(), 3, "one attempt plus two retries");
}

#[test]
fn zero_retries_means_one_attempt() {
    let fake = Fake::start();
    fake.reply_status(503);
    let client = fake.client_with(|c| c.retries(0));
    assert!(client.queues().list_all().next().expect("an item").is_err());
    assert_eq!(fake.count(), 1);
}

#[test]
fn client_and_server_errors_are_not_retried() {
    let fake = Fake::start();
    let client = fake.client();
    for error in [
        ApiError::not_found("job", job_id(1)),
        ApiError::unauthenticated("bad token"),
        ApiError::internal("bug"),
        ApiError::conflict("busy"),
    ] {
        fake.reply_error(error.clone());
        let err = client.jobs().get(job_id(1)).unwrap_err();
        assert_eq!(err.api_error(), Some(&error));
    }
    assert_eq!(fake.count(), 4);
}

#[test]
fn plain_enqueue_is_not_retried_after_the_server_saw_it() {
    let fake = Fake::start();
    fake.reply_error(unavailable());
    fake.reply_json(200, &created());

    let err = fake
        .client()
        .jobs()
        .enqueue(&JobSpec::new("emails", "send"))
        .unwrap_err();

    assert_eq!(err.code(), Some(ErrorCode::Unavailable));
    assert_eq!(fake.count(), 1, "a repeat could create a second job");
    assert_eq!(fake.pending_replies(), 1);
}

#[test]
fn enqueue_with_a_lasting_unique_key_is_retried() {
    let fake = Fake::start();
    fake.reply_status(503);
    fake.reply_json(200, &created());
    let spec = JobSpec::new("emails", "send").unique("user:42", UniqueScope::UntilFinished);

    fake.client()
        .jobs()
        .enqueue(&spec)
        .expect("second attempt works");

    let seen = fake.requests();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].body, seen[1].body, "the same spec is resent");
}

#[test]
fn a_batch_is_retried_only_when_every_spec_is_repeatable() {
    let fake = Fake::start();
    let unique = JobSpec::new("q", "k").unique("a", UniqueScope::UntilFinished);
    let plain = JobSpec::new("q", "k");
    fake.reply_status(503);
    let err = fake
        .client()
        .jobs()
        .enqueue_batch(&[unique.clone(), plain])
        .unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::Unavailable));
    assert_eq!(fake.count(), 1);
}
