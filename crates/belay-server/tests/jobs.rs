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
