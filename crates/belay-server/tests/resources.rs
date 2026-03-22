//! Queues, schedules, calendars, workflows, workers, cluster members,
//! identity and administration, through the typed client (which is what
//! the command line uses) and raw requests where status codes matter.

mod common;

use std::time::Duration;

use belay_client::ListOptions;
use belay_common::Timestamp;
use belay_http::Method;
use belay_proto::api::ErrorCode;
use belay_proto::workflow::{StepSpec, WorkflowState};
use belay_proto::{
    JobSpec, JobState, QueueConfig, QueueName, ScheduleId, ScheduleSpec, WorkflowSpec,
};
use common::{api_error, json, TestServer};

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid timestamp")
}

#[test]
fn queues_are_listed_configured_paused_purged_and_deleted() {
    let server = TestServer::start();
    let client = server.client();
    let emails = QueueName::from_static("emails");
    for _ in 0..3 {
        client
            .jobs()
            .enqueue(&JobSpec::new("emails", "send"))
            .unwrap();
    }
    client.jobs().enqueue(&JobSpec::new("sms", "send")).unwrap();

    let page = client.queues().list(&ListOptions::new().limit(1)).unwrap();
    assert_eq!(page.items[0].name, emails);
    let all: Vec<_> = client
        .queues()
        .list_all()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(all.len(), 2);

    let config = QueueConfig {
        max_concurrency: Some(4),
        ..QueueConfig::default()
    };
    let info = client.queues().set_config(&emails, &config).unwrap();
    assert_eq!(info.config.max_concurrency, Some(4));

    client.queues().pause(&emails).unwrap();
    assert!(client.queues().get(&emails).unwrap().paused);
    let paused = server.post("/v1/queues/emails/resume", "", None);
    assert_eq!(paused.status, 200);
    assert_eq!(json(&paused)["paused"], false);

    assert_eq!(
        client
            .queues()
            .purge(&emails, Some(JobState::Ready))
            .unwrap(),
        3
    );
    let refused = server.send(Method::Delete, "/v1/queues/sms", "", None);
    assert_eq!(api_error(&refused).code, ErrorCode::FailedPrecondition);
    let forced = server.send(Method::Delete, "/v1/queues/sms?force=true", "", None);
    assert_eq!(forced.status, 200);
    assert_eq!(json(&forced)["purged"], 1);

    let bad = api_error(&server.post("/v1/queues/emails/purge?state=finished", "", None));
    assert!(bad.message.starts_with("$.state: "), "{}", bad.message);
    let invalid = api_error(&server.get("/v1/queues/Not%20Valid", None));
    assert_eq!(invalid.code, ErrorCode::InvalidArgument);
}

#[test]
fn schedules_are_created_once_and_upserted_by_put() {
    let server = TestServer::start();
    let client = server.client();
    let spec = ScheduleSpec::new(
        "daily-report".parse().unwrap(),
        "0 9 * * *",
        JobSpec::new("reports", "build"),
    );
    let body = belay_json::to_string_of(&spec);
    let created = server.post("/v1/schedules", &body, None);
    assert_eq!(created.status, 201);
    assert_eq!(
        created.header("location"),
        Some("/v1/schedules/daily-report")
    );
    let again = server.post("/v1/schedules", &body, None);
    assert_eq!(again.status, 409);
    assert_eq!(api_error(&again).code, ErrorCode::AlreadyExists);

    // PUT may leave the id out: the path names it.
    let put = server.send(
        Method::Put,
        "/v1/schedules/nightly",
        r#"{"cron": "0 3 * * *", "template": {"queue": "reports", "kind": "nightly"}}"#,
        None,
    );
    assert_eq!(put.status, 201);
    let replaced = server.send(
        Method::Put,
        "/v1/schedules/nightly",
        r#"{"id": "nightly", "cron": "0 4 * * *", "template": {"queue": "reports", "kind": "nightly"}}"#,
        None,
    );
    assert_eq!(replaced.status, 200);
    let mismatch = server.send(
        Method::Put,
        "/v1/schedules/nightly",
        r#"{"id": "other", "cron": "0 4 * * *", "template": {"queue": "q", "kind": "k"}}"#,
        None,
    );
    assert_eq!(api_error(&mismatch).code, ErrorCode::InvalidArgument);

    let ids: Vec<String> = client
        .schedules()
        .list_all()
        .map(|s| s.map(|s| s.spec.id.to_string()))
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(ids, ["daily-report", "nightly"]);
}
