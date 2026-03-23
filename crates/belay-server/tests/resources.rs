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

#[test]
fn schedules_preview_pause_trigger_and_delete() {
    let server = TestServer::start();
    let client = server.client();
    let id: ScheduleId = "daily-report".parse().unwrap();
    let spec = ScheduleSpec::new(id.clone(), "0 9 * * *", JobSpec::new("reports", "build"));
    client.schedules().create(&spec).unwrap();

    let times = client.schedules().preview(&id, 3).unwrap();
    assert_eq!(
        times,
        [
            ts("2025-03-03T09:00:00Z"),
            ts("2025-03-04T09:00:00Z"),
            ts("2025-03-05T09:00:00Z")
        ]
    );
    let too_many = api_error(&server.get("/v1/schedules/daily-report/preview?n=5000", None));
    assert_eq!(
        too_many.message,
        "$.n: must be between 1 and 1000, got 5000"
    );

    client.schedules().pause(&id).unwrap();
    assert!(client.schedules().get(&id).unwrap().spec.paused);
    client.schedules().resume(&id).unwrap();

    let job = client.schedules().trigger(&id).unwrap();
    assert_eq!(client.jobs().get(job).unwrap().spec.kind, "build");

    let deleted = server.send(Method::Delete, "/v1/schedules/daily-report", "", None);
    assert_eq!(deleted.status, 200);
    assert_eq!(json(&deleted)["spec"]["id"], "daily-report");
    assert!(client.schedules().get(&id).unwrap_err().is_not_found());
}

#[test]
fn calendars_are_put_listed_and_protected_while_in_use() {
    let server = TestServer::start();
    let client = server.client();
    let put = server.send(
        Method::Put,
        "/v1/calendars/business",
        r#"{"timezone": "UTC", "exclude_weekdays": "sat,sun"}"#,
        None,
    );
    assert_eq!(put.status, 201, "{}", String::from_utf8_lossy(&put.body));
    assert_eq!(put.header("location"), Some("/v1/calendars/business"));
    let calendar = client.calendars().get("business").unwrap();
    assert_eq!(calendar.name, "business");
    let again = client.calendars().put(&calendar).unwrap();
    assert_eq!(again, calendar);
    assert_eq!(
        client
            .calendars()
            .list(&ListOptions::new())
            .unwrap()
            .items
            .len(),
        1
    );

    let spec = ScheduleSpec::new(
        "weekdays".parse().unwrap(),
        "0 9 * * *",
        JobSpec::new("reports", "build"),
    )
    .calendar("business");
    client.schedules().create(&spec).unwrap();
    let in_use = client.calendars().delete("business").unwrap_err();
    assert_eq!(in_use.code(), Some(ErrorCode::FailedPrecondition));
    client.schedules().delete(&spec.id).unwrap();
    client.calendars().delete("business").unwrap();
    assert!(client
        .calendars()
        .get("business")
        .unwrap_err()
        .is_not_found());
}

#[test]
fn workflows_are_submitted_listed_and_cancelled() {
    let server = TestServer::start();
    let client = server.client();
    let spec = WorkflowSpec::new("etl")
        .step(StepSpec::new("extract", JobSpec::new("etl", "extract")))
        .step(StepSpec::new("load", JobSpec::new("etl", "load")).after("extract"));
    let body = belay_json::to_string_of(&spec);
    let created = server.post("/v1/workflows", &body, None);
    assert_eq!(created.status, 201);
    let id = json(&created)["id"].as_str().expect("id").to_owned();
    assert_eq!(
        created.header("location"),
        Some(format!("/v1/workflows/{id}").as_str())
    );
    let id = id.parse().unwrap();

    let run = client.workflows().get(id).unwrap();
    assert_eq!(run.state, WorkflowState::Running);
    let details = json(&server.get(&format!("/v1/workflows/{id}?details=true"), None));
    assert_eq!(details["workflow"]["id"], id.to_string().as_str());

    let second = client.workflows().submit(&spec).unwrap();
    let page = client
        .workflows()
        .list(&ListOptions::new().limit(1))
        .unwrap();
    assert_eq!(page.items[0].id, second.id, "newest first");
    let running = server.get("/v1/workflows?state=running&name=etl", None);
    assert_eq!(json(&running)["items"].as_array().map(Vec::len), Some(2));

    client.workflows().cancel(id).unwrap();
    assert_eq!(
        client.workflows().get(id).unwrap().state,
        WorkflowState::Cancelled
    );
    let invalid = WorkflowSpec::new("broken")
        .step(StepSpec::new("a", JobSpec::new("q", "k")).after("missing"));
    let refused = server.post("/v1/workflows", &belay_json::to_string_of(&invalid), None);
    assert_eq!(api_error(&refused).code, ErrorCode::InvalidArgument);
}

#[test]
fn info_members_and_metrics_describe_the_node() {
    let server = TestServer::start();
    let client = server.client();
    let info = client.info().unwrap();
    assert_eq!(info.version, belay_core::VERSION);
    assert_eq!(info.storage, "memory");
    assert_eq!(info.cluster_size, 1);
    client.health().unwrap();
    assert!(client.ready().unwrap());

    let members = client.members().unwrap();
    assert_eq!(members.len(), 1);
    assert!(members[0].leader);
    assert_eq!(members[0].id, server.engine.node_id());

    client
        .jobs()
        .enqueue(&JobSpec::new("emails", "send"))
        .unwrap();
    let scrape = server.get("/metrics", None);
    assert_eq!(
        scrape.header("content-type"),
        Some(belay_metrics::text::CONTENT_TYPE)
    );
    let text = String::from_utf8(scrape.body).expect("utf-8");
    assert!(
        text.contains(
            "belay_http_requests_total{route=\"/v1/jobs\",method=\"POST\",status=\"201\"} 1"
        ),
        "{text}"
    );
    assert!(
        text.contains("belay_jobs_enqueued_total{queue=\"emails\"} 1"),
        "{text}"
    );
    assert!(text.contains("belay_http_request_duration_seconds_bucket{route=\"/v1/info\""));
}

#[test]
fn administration_checkpoints_reports_and_verifies() {
    let server = TestServer::start();
    let client = server.client();
    client
        .jobs()
        .enqueue(&JobSpec::new("emails", "send"))
        .unwrap();
    let checkpoint = client.admin().checkpoint().unwrap();
    assert!(checkpoint.took < Duration::from_secs(10));
    let store = client.admin().store().unwrap();
    assert_eq!(store["storage"], "memory");
    let verify = json(&server.get("/v1/admin/verify", None));
    assert_eq!(verify["consistent"], true);
    assert_eq!(verify["jobs"], 1);
}
