//! Job endpoints and the worker protocol: the exact request each method
//! sends, and how each response is decoded.

mod common;

use std::time::Duration;

use belay_client::Error;
use belay_common::Timestamp;
use belay_json::{json, ToJson, Value};
use belay_proto::api::{
    ApiError, CompleteRequest, Cursor, EnqueueResponse, EnqueueResult, EnqueueStatus, ErrorCode,
    FailRequest, HeartbeatRequest, HeartbeatResponse, JobFilter, LeaseRequest, LeaseResponse,
    LeasedJob, Page, SortOrder,
};
use belay_proto::{
    JobSource, JobSpec, JobState, LeaseToken, Progress, QueueName, UniqueScope, WorkerId,
};
use common::{job_id, sample_job, t0, Fake, TOKEN};

fn created(n: u8) -> EnqueueResult {
    EnqueueResult {
        id: job_id(n),
        status: EnqueueStatus::Created,
        state: JobState::Ready,
    }
}

fn token() -> LeaseToken {
    LeaseToken::new(0xc0de)
}

#[test]
fn enqueue_posts_the_bare_spec_with_auth_and_json_headers() {
    let fake = Fake::start();
    fake.reply_json(
        201,
        &EnqueueResponse {
            results: vec![created(1)],
        },
    );
    let spec = JobSpec::new("emails", "send_welcome")
        .payload(json!({"user": 42}))
        .priority(10)
        .tag("signup");

    let result = fake.client().jobs().enqueue(&spec).expect("enqueued");

    assert_eq!(result, created(1));
    let seen = fake.only();
    assert_eq!(seen.call(), "POST /v1/jobs");
    assert_eq!(seen.json(), spec.to_json());
    assert_eq!(seen.header("authorization"), Some("Bearer test-token"));
    assert_eq!(seen.header("content-type"), Some("application/json"));
    assert_eq!(seen.header("accept"), Some("application/json"));
    let agent = seen.header("user-agent").expect("user agent sent");
    assert!(agent.starts_with("belay-client/"), "{agent}");
}

#[test]
fn requests_carry_no_authorization_without_a_token() {
    let fake = Fake::start();
    fake.reply_json(200, &sample_job(1));
    let client = fake.client_with(|mut c| {
        c.token = None;
        c
    });
    client.jobs().get(job_id(1)).expect("job");
    assert_eq!(fake.only().header("authorization"), None);
}

#[test]
fn enqueue_batch_wraps_specs_and_keeps_result_order() {
    let fake = Fake::start();
    let duplicate = EnqueueResult {
        status: EnqueueStatus::Duplicate,
        state: JobState::Running,
        ..created(7)
    };
    fake.reply_json(
        200,
        &EnqueueResponse {
            results: vec![created(1), duplicate.clone()],
        },
    );
    let specs = [
        JobSpec::new("emails", "send"),
        JobSpec::new("sms", "send").unique("user:7", UniqueScope::UntilFinished),
    ];

    let results = fake
        .client()
        .jobs()
        .enqueue_batch(&specs)
        .expect("enqueued");

    assert_eq!(results, [created(1), duplicate]);
    let body = fake.only().json();
    assert_eq!(body["jobs"].as_array().map(Vec::len), Some(2));
    assert_eq!(body["jobs"][1], specs[1].to_json());
}

#[test]
fn empty_batch_sends_nothing() {
    let fake = Fake::start();
    assert!(fake
        .client()
        .jobs()
        .enqueue_batch(&[])
        .expect("ok")
        .is_empty());
    assert_eq!(fake.count(), 0);
}

#[test]
fn result_count_mismatch_is_a_decode_error() {
    let fake = Fake::start();
    fake.reply_json(
        200,
        &EnqueueResponse {
            results: vec![created(1)],
        },
    );
    let specs = [JobSpec::new("q", "a"), JobSpec::new("q", "b")];
    let err = fake.client().jobs().enqueue_batch(&specs).unwrap_err();
    assert!(matches!(err, Error::Decode(_)), "{err}");
    assert_eq!(
        err.to_string(),
        "unexpected response to POST /v1/jobs: expected 2 results for 2 jobs, got 1"
    );

    fake.reply_json(200, &EnqueueResponse { results: vec![] });
    let err = fake.client().jobs().enqueue(&specs[0]).unwrap_err();
    assert_eq!(
        err.to_string(),
        "unexpected response to POST /v1/jobs: expected 1 result for 1 job, got 0"
    );
}

#[test]
fn rejected_spec_surfaces_the_servers_violations() {
    let fake = Fake::start();
    let error = ApiError::invalid_argument("$.priority: must be between -1000 and 1000, got 5000")
        .with_details(json!({"violations": [{"field": "$.priority", "message": "too high"}]}));
    fake.reply_error(error.clone());
    let err = fake
        .client()
        .jobs()
        .enqueue(&JobSpec::new("q", "k").priority(5000))
        .unwrap_err();
    assert_eq!(err.api_error(), Some(&error));
    assert_eq!(fake.count(), 1, "client errors are not retried");
}
