//! `/v1/jobs` through the typed client and raw requests: enqueueing in
//! both body forms, uniqueness, listing with filters and cursors, and the
//! management actions.

mod common;

use std::time::Duration;

use belay_client::Error;
use belay_http::Method;
use belay_proto::api::{EnqueueStatus, ErrorCode, JobFilter};
use belay_proto::{JobSpec, JobState, QueueName, UniqueScope};
use common::{api_error, json, TestServer};

#[test]
fn a_single_spec_is_created_with_a_location() {
    let server = TestServer::start();
    let response = server.post(
        "/v1/jobs",
        r#"{"queue": "emails", "kind": "send_welcome", "payload": {"user": 42}}"#,
        None,
    );
    assert_eq!(response.status, 201);
    let body = json(&response);
    let id = body["results"][0]["id"].as_str().expect("an id").to_owned();
    assert_eq!(body["results"][0]["status"], "created");
    assert_eq!(body["results"][0]["state"], "ready");
    assert_eq!(
        response.header("location"),
        Some(format!("/v1/jobs/{id}").as_str())
    );

    let job = server.client().jobs().get(id.parse().unwrap()).unwrap();
    assert_eq!(job.spec.kind, "send_welcome");
    assert_eq!(job.spec.payload["user"], 42);
}

#[test]
fn batches_return_one_result_per_spec_in_order() {
    let server = TestServer::start();
    let specs = vec![
        JobSpec::new("emails", "send").priority(5),
        JobSpec::new("sms", "send").delay(Duration::from_secs(60)),
    ];
    let results = server.client().jobs().enqueue_batch(&specs).unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].state, JobState::Ready);
    assert_eq!(results[1].state, JobState::Scheduled);
    // A batch has no single Location.
    let response = server.post(
        "/v1/jobs",
        r#"{"jobs": [{"queue": "emails", "kind": "a"}, {"queue": "emails", "kind": "b"}]}"#,
        None,
    );
    assert_eq!(response.status, 201);
    assert_eq!(response.header("location"), None);
}

#[test]
fn duplicates_are_answered_200_with_the_existing_job() {
    let server = TestServer::start();
    let client = server.client();
    let spec = JobSpec::new("emails", "welcome").unique("welcome:42", UniqueScope::UntilFinished);
    let first = client.jobs().enqueue(&spec).unwrap();
    assert_eq!(first.status, EnqueueStatus::Created);
    let body = belay_json::to_string_of(&spec);
    let again = server.post("/v1/jobs", &body, None);
    assert_eq!(again.status, 200);
    let result = &json(&again)["results"][0];
    assert_eq!(result["status"], "duplicate");
    assert_eq!(result["id"], first.id.to_string().as_str());
}

#[test]
fn invalid_specs_are_refused_with_every_violation() {
    let server = TestServer::start();
    let response = server.post(
        "/v1/jobs",
        r#"{"jobs": [{"queue": "Bad Queue", "kind": "k"}, {"queue": "q", "kind": "k", "priority": 5000}]}"#,
        None,
    );
    assert_eq!(response.status, 400);
    let error = api_error(&response);
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert!(
        error.message.contains("$.jobs[0].queue"),
        "{}",
        error.message
    );
}

#[test]
fn listings_filter_and_page_with_cursors() {
    let server = TestServer::start();
    let client = server.client();
    for i in 0..5 {
        client
            .jobs()
            .enqueue(&JobSpec::new("emails", format!("k{i}")).tag("batch-1"))
            .unwrap();
    }
    client
        .jobs()
        .enqueue(&JobSpec::new("sms", "other"))
        .unwrap();

    let filter = JobFilter::new()
        .queue(QueueName::from_static("emails"))
        .limit(2);
    let first = client.jobs().list(&filter).unwrap();
    assert_eq!(first.items.len(), 2);
    let cursor = first.next_cursor.clone().expect("more pages");
    let second = client.jobs().list(&filter.clone().after(cursor)).unwrap();
    assert_eq!(second.items.len(), 2);
    assert!(first
        .items
        .iter()
        .all(|j| !second.items.iter().any(|k| k.id == j.id)));

    let all: Vec<_> = client
        .jobs()
        .list_all(
            JobFilter::new()
                .queue(QueueName::from_static("emails"))
                .limit(2),
        )
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(all.len(), 5);
    let tagged = client
        .jobs()
        .list(&JobFilter::new().tag("batch-1"))
        .unwrap();
    assert_eq!(tagged.items.len(), 5);
    let sms = client
        .jobs()
        .list(&JobFilter::new().state(JobState::Ready).kind("other"))
        .unwrap();
    assert_eq!(sms.items.len(), 1);
}

#[test]
fn listings_refuse_unknown_parameters_and_bad_values() {
    let server = TestServer::start();
    let typo = api_error(&server.get("/v1/jobs?queu=emails", None));
    assert!(typo.message.contains("queu"), "{}", typo.message);
    let bad_state = server.get("/v1/jobs?state=done", None);
    assert_eq!(bad_state.status, 400);
    let bad_limit = api_error(&server.get("/v1/jobs?limit=0", None));
    assert!(bad_limit.message.contains("limit"), "{}", bad_limit.message);
}

#[test]
fn cancel_retry_promote_and_delete() {
    let server = TestServer::start();
    let jobs = server.client();
    let jobs = jobs.jobs();
    let waiting = jobs
        .enqueue(&JobSpec::new("emails", "later").delay(Duration::from_secs(3600)))
        .unwrap()
        .id;
    let promoted = jobs.promote(waiting).unwrap();
    assert_eq!(promoted.state, JobState::Ready);

    let cancelled = jobs.cancel(waiting).unwrap();
    assert_eq!(cancelled.state, JobState::Cancelled);
    let retried = jobs.retry(waiting).unwrap();
    assert_eq!(retried.state, JobState::Ready);

    // Only finished jobs can be deleted.
    let err = jobs.delete(waiting).unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::FailedPrecondition));
    jobs.cancel(waiting).unwrap();
    let deleted = server.send(Method::Delete, &format!("/v1/jobs/{waiting}"), "", None);
    assert_eq!(deleted.status, 204);
    assert!(jobs.get(waiting).unwrap_err().is_not_found());
}
