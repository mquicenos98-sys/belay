//! The worker protocol over HTTP: leasing (including long polls),
//! heartbeats, results, stale tokens, the client as a `JobSource`, and the
//! worker runtime draining a queue through all of it.

mod common;

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use belay_json::json;
use belay_proto::api::{CompleteRequest, ErrorCode, FailRequest, HeartbeatRequest, LeaseRequest};
use belay_proto::{JobId, JobSource, JobSpec, JobState, QueueName, WorkerId};
use belay_worker::{handler_fn, HandlerError, Outcome, Registry, Worker, WorkerConfig};
use common::{api_error, eventually, json, TestServer};

fn lease_request(queues: &[&'static str]) -> LeaseRequest {
    LeaseRequest::new(
        WorkerId::from_static("w1"),
        queues.iter().map(|q| QueueName::from_static(q)).collect(),
    )
}

#[test]
fn a_job_goes_from_lease_to_success() {
    let server = TestServer::start();
    let client = server.client();
    let id = client
        .jobs()
        .enqueue(&JobSpec::new("emails", "send"))
        .unwrap()
        .id;
    let leased = client.jobs().lease(&lease_request(&["emails"])).unwrap();
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].job.id, id);
    assert_eq!(leased[0].job.state, JobState::Running);

    let mut beat = HeartbeatRequest::new(leased[0].token);
    beat.extend_by = Some(Duration::from_secs(60));
    let answer = client.jobs().heartbeat(id, &beat).unwrap();
    assert!(!answer.cancel_requested);

    let mut done = CompleteRequest::new(leased[0].token);
    done.result = Some(json!({"sent": true}));
    client.jobs().complete(id, &done).unwrap();
    let job = client.jobs().get(id).unwrap();
    assert_eq!(job.state, JobState::Succeeded);
    assert_eq!(job.result, Some(json!({"sent": true})));
    // Read endpoints never show the lease token.
    let raw = server.get(&format!("/v1/jobs/{id}"), None);
    assert!(!String::from_utf8_lossy(&raw.body).contains(&leased[0].token.to_string()));
}

#[test]
fn complete_and_fail_answer_no_content() {
    let server = TestServer::start();
    let client = server.client();
    let id = client
        .jobs()
        .enqueue(&JobSpec::new("emails", "send").max_attempts(1))
        .unwrap()
        .id;
    let leased = client.jobs().lease(&lease_request(&["emails"])).unwrap();
    let body = belay_json::to_string_of(&FailRequest::new(leased[0].token, "smtp down"));
    let response = server.post(&format!("/v1/jobs/{id}/fail"), &body, None);
    assert_eq!(response.status, 204);
    assert!(response.body.is_empty());
    assert_eq!(client.jobs().get(id).unwrap().state, JobState::Dead);
}

#[test]
fn stale_tokens_lose_the_lease() {
    let server = TestServer::start();
    let client = server.client();
    let id = client
        .jobs()
        .enqueue(&JobSpec::new("emails", "send"))
        .unwrap()
        .id;
    let leased = client.jobs().lease(&lease_request(&["emails"])).unwrap();
    client.jobs().cancel(id).unwrap();
    let beat = client
        .jobs()
        .heartbeat(id, &HeartbeatRequest::new(leased[0].token))
        .unwrap();
    assert!(beat.cancel_requested);

    let wrong = format!(r#"{{"token": "{}"}}"#, "0123456789abcdef");
    let response = server.post(&format!("/v1/jobs/{id}/complete"), &wrong, None);
    assert_eq!(response.status, 409);
    assert_eq!(api_error(&response).code, ErrorCode::LeaseLost);
}
