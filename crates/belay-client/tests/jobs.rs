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

#[test]
fn get_decodes_the_job_and_reports_missing_ones() {
    let fake = Fake::start();
    let mut job = sample_job(3);
    job.state = JobState::Dead;
    job.attempt = 25;
    fake.reply_json(200, &job);
    let client = fake.client();

    assert_eq!(client.jobs().get(job_id(3)).expect("job"), job);
    let seen = fake.only();
    assert_eq!(seen.call(), "GET /v1/jobs/:id");
    assert_eq!(seen.param("id"), Some(job_id(3).to_string().as_str()));

    fake.reply_error(ApiError::not_found("job", job_id(4)));
    let err = client.jobs().get(job_id(4)).unwrap_err();
    assert!(err.is_not_found(), "{err}");
    assert_eq!(
        err.to_string(),
        format!("server error: not_found: job {} not found", job_id(4))
    );
}

#[test]
fn list_sends_the_filter_as_query_parameters() {
    let fake = Fake::start();
    let next = Cursor::after_job(job_id(2));
    fake.reply_json(
        200,
        &Page::new(vec![sample_job(1), sample_job(2)], Some(next.clone())),
    );
    let filter = JobFilter::new()
        .queue(QueueName::from_static("emails"))
        .state(JobState::Dead)
        .state(JobState::Retrying)
        .kind("send")
        .tag("signup")
        .created_between(Some(t0()), None)
        .order(SortOrder::Asc)
        .limit(2);

    let page = fake.client().jobs().list(&filter).expect("page");

    assert_eq!(page.items.len(), 2);
    assert_eq!(page.next_cursor, Some(next));
    let seen = fake.only();
    assert_eq!(seen.call(), "GET /v1/jobs");
    assert_eq!(
        seen.query_pairs(),
        [
            ("queue", "emails"),
            ("state", "dead,retrying"),
            ("kind", "send"),
            ("tag", "signup"),
            ("created_after", "2025-03-04T05:06:07.089Z"),
            ("order", "asc"),
            ("limit", "2"),
        ]
    );
    // What the server would parse back is the filter we sent.
    let pairs: Vec<(String, String)> = seen.query.clone();
    assert_eq!(JobFilter::from_query(&pairs), Ok(filter));
}

#[test]
fn list_all_follows_cursors_to_the_last_page() {
    let fake = Fake::start();
    let first = Cursor::after_job(job_id(2));
    let second = Cursor::after_job(job_id(4));
    fake.reply_json(
        200,
        &Page::new(vec![sample_job(1), sample_job(2)], Some(first.clone())),
    );
    fake.reply_json(
        200,
        &Page::new(vec![sample_job(3), sample_job(4)], Some(second.clone())),
    );
    fake.reply_json(200, &Page::last(vec![sample_job(5)]));

    let client = fake.client();
    let mut all = client
        .jobs()
        .list_all(JobFilter::new().state(JobState::Ready).limit(2));
    let ids: Vec<_> = all.by_ref().map(|j| j.expect("job").id).collect();

    assert_eq!(ids, (1..=5).map(job_id).collect::<Vec<_>>());
    assert_eq!(all.pages_fetched(), 3);
    assert!(all.cursor().is_none());
    let queries: Vec<Vec<(String, String)>> =
        fake.requests().into_iter().map(|s| s.query).collect();
    let pair = |k: &str, v: String| (k.to_owned(), v);
    assert_eq!(
        queries,
        [
            vec![pair("state", "ready".into()), pair("limit", "2".into())],
            vec![
                pair("state", "ready".into()),
                pair("limit", "2".into()),
                pair("cursor", first.to_string())
            ],
            vec![
                pair("state", "ready".into()),
                pair("limit", "2".into()),
                pair("cursor", second.to_string())
            ],
        ]
    );
}

#[test]
fn list_all_starts_at_the_filters_cursor_and_fetches_lazily() {
    let fake = Fake::start();
    let start = Cursor::after_job(job_id(9));
    fake.reply_json(
        200,
        &Page::new(vec![sample_job(8)], Some(Cursor::after_job(job_id(8)))),
    );

    let client = fake.client();
    let first: Vec<_> = client
        .jobs()
        .list_all(JobFilter::new().after(start.clone()))
        .take(1)
        .collect();

    assert_eq!(first.len(), 1);
    let seen = fake.only();
    assert_eq!(seen.query_pairs(), [("cursor", start.to_string().as_str())]);
}

#[test]
fn list_all_yields_a_failed_page_once_and_ends() {
    let fake = Fake::start();
    fake.reply_json(
        200,
        &Page::new(vec![sample_job(1)], Some(Cursor::after_job(job_id(1)))),
    );
    fake.reply_error(ApiError::permission_denied("token lacks the read scope"));

    let items: Vec<_> = fake.client().jobs().list_all(JobFilter::new()).collect();

    assert_eq!(items.len(), 2);
    assert_eq!(
        items[1].as_ref().unwrap_err().code(),
        Some(ErrorCode::PermissionDenied)
    );
}

#[test]
fn management_actions_post_to_their_routes() {
    let fake = Fake::start();
    let client = fake.client();
    let mut cancelled = sample_job(1);
    cancelled.state = JobState::Cancelled;
    for _ in 0..3 {
        fake.reply_json(200, &cancelled);
    }
    fake.reply_status(204);

    assert_eq!(client.jobs().cancel(job_id(1)).expect("cancel"), cancelled);
    client.jobs().retry(job_id(1)).expect("retry");
    client.jobs().promote(job_id(1)).expect("promote");
    client.jobs().delete(job_id(1)).expect("delete");

    let calls: Vec<String> = fake.requests().iter().map(|s| s.call()).collect();
    assert_eq!(
        calls,
        [
            "POST /v1/jobs/:id/cancel",
            "POST /v1/jobs/:id/retry",
            "POST /v1/jobs/:id/promote",
            "DELETE /v1/jobs/:id",
        ]
    );
    for seen in fake.requests() {
        assert_eq!(seen.param("id"), Some(job_id(1).to_string().as_str()));
        assert!(seen.body.is_empty(), "{} has no body", seen.call());
    }
}

#[test]
fn deleting_a_running_job_reports_the_precondition() {
    let fake = Fake::start();
    fake.reply_error(ApiError::failed_precondition("job is running"));
    let err = fake.client().jobs().delete(job_id(1)).unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::FailedPrecondition));
    assert_eq!(fake.count(), 1);
}

#[test]
fn lease_sends_the_request_and_returns_the_leased_jobs() {
    let fake = Fake::start();
    let mut job = sample_job(5);
    job.state = JobState::Running;
    job.attempt = 1;
    fake.reply_json(
        200,
        &LeaseResponse {
            jobs: vec![LeasedJob {
                job: job.clone(),
                token: token(),
            }],
        },
    );
    let request = LeaseRequest::new(
        WorkerId::from_static("host-1:4242"),
        vec![
            QueueName::from_static("emails"),
            QueueName::from_static("sms"),
        ],
    )
    .max_jobs(4)
    .lease_for(Duration::from_secs(60))
    .wait(Duration::from_millis(100));

    let leased = fake.client().jobs().lease(&request).expect("leased");

    assert_eq!(
        leased,
        [LeasedJob {
            job,
            token: token()
        }]
    );
    let seen = fake.only();
    assert_eq!(seen.call(), "POST /v1/lease");
    assert_eq!(
        seen.json(),
        json!({
            "worker": "host-1:4242",
            "queues": ["emails", "sms"],
            "max_jobs": 4,
            "lease_for": "1m",
            "wait": "100ms"
        })
    );
}

#[test]
fn empty_lease_answer_means_nothing_was_ready() {
    let fake = Fake::start();
    fake.reply_json(200, &json!({}));
    let request = LeaseRequest::new(
        WorkerId::from_static("w1"),
        vec![QueueName::from_static("q")],
    );
    assert!(fake.client().jobs().lease(&request).expect("ok").is_empty());
}

#[test]
fn worker_reports_send_the_token_and_decode_answers() {
    let fake = Fake::start();
    let client = fake.client();
    let expires_at = Timestamp::from_millis(t0().as_millis() + 30_000);
    fake.reply_json(
        200,
        &HeartbeatResponse {
            expires_at,
            cancel_requested: true,
        },
    );
    fake.reply_status(204);
    fake.reply_status(204);

    let heartbeat = HeartbeatRequest {
        token: token(),
        extend_by: Some(Duration::from_secs(90)),
        progress: Some(Progress::percent(40.0).with_message("halfway-ish")),
    };
    let answer = client
        .jobs()
        .heartbeat(job_id(2), &heartbeat)
        .expect("heartbeat");
    assert!(answer.cancel_requested);
    assert_eq!(answer.expires_at, expires_at);

    let complete = CompleteRequest {
        token: token(),
        result: Some(Value::Null),
    };
    client
        .jobs()
        .complete(job_id(2), &complete)
        .expect("complete");
    let fail = FailRequest {
        kind: Some("timeout".into()),
        retryable: false,
        retry_in: Some(Duration::from_secs(5)),
        ..FailRequest::new(token(), "upstream timed out")
    };
    client.jobs().fail(job_id(2), &fail).expect("fail");

    let seen = fake.requests();
    assert_eq!(seen[0].call(), "POST /v1/jobs/:id/heartbeat");
    assert_eq!(
        seen[0].json(),
        json!({"token": "000000000000c0de", "extend_by": "1m30s",
               "progress": {"percent": 40.0, "message": "halfway-ish"}})
    );
    assert_eq!(seen[1].call(), "POST /v1/jobs/:id/complete");
    assert_eq!(
        seen[1].json(),
        json!({"token": "000000000000c0de", "result": null})
    );
    assert_eq!(seen[2].call(), "POST /v1/jobs/:id/fail");
    assert_eq!(
        seen[2].json(),
        json!({"token": "000000000000c0de", "error": "upstream timed out",
               "kind": "timeout", "retryable": false, "retry_in": "5s"})
    );
}

#[test]
fn complete_accepts_a_json_answer_as_well_as_no_content() {
    let fake = Fake::start();
    fake.reply_json(200, &json!({"ok": true}));
    fake.client()
        .jobs()
        .complete(job_id(1), &CompleteRequest::new(token()))
        .expect("a body on success is ignored");
}

#[test]
fn job_source_maps_lease_lost_and_outages_to_api_errors() {
    let fake = Fake::start();
    fake.reply_error(ApiError::lease_lost(job_id(1)));
    let client = fake.client();
    let source: &dyn JobSource = &client;

    let err = source
        .complete(&job_id(1), &CompleteRequest::new(token()))
        .unwrap_err();
    assert_eq!(err, ApiError::lease_lost(job_id(1)));

    let unreachable =
        belay_client::Client::new(common::unreachable_config().retries(0)).expect("config");
    let err = JobSource::lease(
        &unreachable,
        &LeaseRequest::new(
            WorkerId::from_static("w1"),
            vec![QueueName::from_static("q")],
        ),
    )
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable);
    assert!(err.is_retryable());
}

#[test]
fn job_source_heartbeat_and_fail_go_through_the_same_endpoints() {
    let fake = Fake::start();
    fake.reply_json(
        200,
        &HeartbeatResponse {
            expires_at: t0(),
            cancel_requested: false,
        },
    );
    fake.reply_status(204);
    let client = fake.client();
    let answer = JobSource::heartbeat(&client, &job_id(3), &HeartbeatRequest::new(token()))
        .expect("heartbeat");
    assert!(!answer.cancel_requested);
    JobSource::fail(&client, &job_id(3), &FailRequest::new(token(), "boom")).expect("fail");
    let calls: Vec<String> = fake.requests().iter().map(|s| s.call()).collect();
    assert_eq!(
        calls,
        ["POST /v1/jobs/:id/heartbeat", "POST /v1/jobs/:id/fail"]
    );
    assert_eq!(
        fake.last().header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
}
