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
