//! Queues, schedules, calendars, workflows, the server-wide calls and the
//! admin endpoints: request shapes and response decoding.

mod common;

use std::time::Duration;

use belay_client::ListOptions;
use belay_http::{Response, StatusCode};
use belay_json::{json, ToJson};
use belay_proto::api::{
    ApiError, CheckpointResponse, Cursor, ErrorCode, MemberInfo, MemberState, Page,
    PreviewResponse, PurgeResponse, ServerInfo, TriggerResponse, WorkerInfo,
};
use belay_proto::workflow::StepSpec;
use belay_proto::{
    CalendarSpec, JobSpec, JobState, MisfirePolicy, QueueConfig, QueueInfo, QueueName, RateLimit,
    Schedule, ScheduleId, ScheduleSpec, WorkerId, Workflow, WorkflowId, WorkflowSpec,
};
use common::{job_id, t0, Fake};

fn queue(name: &'static str) -> QueueName {
    QueueName::from_static(name)
}

fn schedule_spec() -> ScheduleSpec {
    ScheduleSpec::new(
        ScheduleId::from_static("nightly"),
        "0 2 * * *",
        JobSpec::new("reports", "build"),
    )
    .timezone("America/Bogota")
    .misfire(MisfirePolicy::FireOnce)
}

fn schedule() -> Schedule {
    Schedule::new(schedule_spec(), Some(t0()), t0())
}

fn workflow_id() -> WorkflowId {
    "01HQ3K9Z8N6V2T4R5S7W0X1Y2Z".parse().expect("valid id")
}

// ---- queues -------------------------------------------------------------

#[test]
fn queue_listing_and_status() {
    let fake = Fake::start();
    let info = QueueInfo::new(queue("billing:invoices"));
    fake.reply_json(200, &Page::last(vec![info.clone()]));
    fake.reply_json(200, &info);
    let client = fake.client();

    let page = client
        .queues()
        .list(&ListOptions::new().limit(20))
        .expect("page");
    assert_eq!(page.items, [info.clone()]);
    assert_eq!(
        client
            .queues()
            .get(&queue("billing:invoices"))
            .expect("info"),
        info
    );

    let seen = fake.requests();
    assert_eq!(seen[0].call(), "GET /v1/queues");
    assert_eq!(seen[0].query_pairs(), [("limit", "20")]);
    assert_eq!(seen[1].call(), "GET /v1/queues/:name");
    assert_eq!(seen[1].path, "/v1/queues/billing:invoices");
    assert_eq!(seen[1].param("name"), Some("billing:invoices"));
}

#[test]
fn queue_configuration_is_put_as_json() {
    let fake = Fake::start();
    let config = QueueConfig {
        max_concurrency: Some(8),
        rate_limit: Some(RateLimit::new(5.0, 10)),
        ..QueueConfig::default()
    };
    let mut info = QueueInfo::new(queue("emails"));
    info.config = config.clone();
    fake.reply_json(200, &info);

    let updated = fake
        .client()
        .queues()
        .set_config(&queue("emails"), &config)
        .expect("updated");

    assert_eq!(updated.config, config);
    let seen = fake.only();
    assert_eq!(seen.call(), "PUT /v1/queues/:name/config");
    assert_eq!(seen.json(), config.to_json());
}

#[test]
fn queue_pause_resume_purge_and_delete() {
    let fake = Fake::start();
    fake.reply_status(204);
    fake.reply_json(200, &QueueInfo::new(queue("emails")));
    fake.reply_json(200, &PurgeResponse { purged: 17 });
    fake.reply_json(200, &PurgeResponse { purged: 3 });
    fake.reply_status(204);
    let client = fake.client();
    let emails = queue("emails");

    client.queues().pause(&emails).expect("paused");
    client
        .queues()
        .resume(&emails)
        .expect("resumed, body ignored");
    assert_eq!(
        client
            .queues()
            .purge(&emails, Some(JobState::Dead))
            .expect("purged"),
        17
    );
    assert_eq!(client.queues().purge(&emails, None).expect("purged"), 3);
    client.queues().delete(&emails).expect("deleted");

    let seen = fake.requests();
    let calls: Vec<String> = seen.iter().map(|s| s.call()).collect();
    assert_eq!(
        calls,
        [
            "POST /v1/queues/:name/pause",
            "POST /v1/queues/:name/resume",
            "POST /v1/queues/:name/purge",
            "POST /v1/queues/:name/purge",
            "DELETE /v1/queues/:name",
        ]
    );
    assert_eq!(seen[2].query_pairs(), [("state", "dead")]);
    assert!(seen[3].query.is_empty());
}
