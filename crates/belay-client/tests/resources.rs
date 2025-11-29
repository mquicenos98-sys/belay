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

#[test]
fn queue_list_all_walks_name_cursors() {
    let fake = Fake::start();
    let after_a = Cursor::from_name("a");
    fake.reply_json(
        200,
        &Page::new(vec![QueueInfo::new(queue("a"))], Some(after_a.clone())),
    );
    fake.reply_json(200, &Page::last(vec![QueueInfo::new(queue("b"))]));

    let names: Vec<String> = fake
        .client()
        .queues()
        .list_all()
        .map(|q| q.expect("queue").name.to_string())
        .collect();

    assert_eq!(names, ["a", "b"]);
    assert_eq!(
        fake.last().query_pairs(),
        [("cursor", after_a.to_string().as_str())]
    );
}

// ---- schedules ----------------------------------------------------------

#[test]
fn schedule_create_put_get_and_delete() {
    let fake = Fake::start();
    fake.reply_json(201, &schedule());
    fake.reply_json(200, &schedule());
    fake.reply_json(200, &schedule());
    fake.reply_status(204);
    let client = fake.client();
    let spec = schedule_spec();

    assert_eq!(
        client.schedules().create(&spec).expect("created"),
        schedule()
    );
    client.schedules().put(&spec).expect("replaced");
    assert_eq!(
        client.schedules().get(&spec.id).expect("got").id(),
        &spec.id
    );
    client.schedules().delete(&spec.id).expect("deleted");

    let seen = fake.requests();
    let calls: Vec<String> = seen.iter().map(|s| s.call()).collect();
    assert_eq!(
        calls,
        [
            "POST /v1/schedules",
            "PUT /v1/schedules/:id",
            "GET /v1/schedules/:id",
            "DELETE /v1/schedules/:id",
        ]
    );
    assert_eq!(seen[0].json(), spec.to_json());
    assert_eq!(seen[1].json(), spec.to_json());
    assert_eq!(seen[1].param("id"), Some("nightly"));
}

#[test]
fn creating_an_existing_schedule_is_already_exists_and_not_retried() {
    let fake = Fake::start();
    fake.reply_error(ApiError::already_exists("schedule", "nightly"));
    let err = fake
        .client()
        .schedules()
        .create(&schedule_spec())
        .unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::AlreadyExists));
    assert_eq!(fake.count(), 1);
}

#[test]
fn schedule_actions_trigger_and_preview() {
    let fake = Fake::start();
    let id = ScheduleId::from_static("nightly");
    fake.reply_status(204);
    fake.reply_status(204);
    fake.reply_json(200, &TriggerResponse { job: job_id(9) });
    let times = vec![t0(), t0() + Duration::from_secs(86_400)];
    fake.reply_json(
        200,
        &PreviewResponse {
            times: times.clone(),
        },
    );
    let client = fake.client();

    client.schedules().pause(&id).expect("paused");
    client.schedules().resume(&id).expect("resumed");
    assert_eq!(
        client.schedules().trigger(&id).expect("triggered"),
        job_id(9)
    );
    assert_eq!(client.schedules().preview(&id, 2).expect("preview"), times);

    let seen = fake.requests();
    let calls: Vec<String> = seen.iter().map(|s| s.call()).collect();
    assert_eq!(
        calls,
        [
            "POST /v1/schedules/:id/pause",
            "POST /v1/schedules/:id/resume",
            "POST /v1/schedules/:id/trigger",
            "GET /v1/schedules/:id/preview",
        ]
    );
    assert_eq!(seen[3].query_pairs(), [("n", "2")]);
}

#[test]
fn schedule_listing_pages_and_walks() {
    let fake = Fake::start();
    let cursor = Cursor::from_name("nightly");
    fake.reply_json(200, &Page::new(vec![schedule()], Some(cursor.clone())));
    fake.reply_json(200, &Page::new(vec![schedule()], Some(cursor.clone())));
    fake.reply_json(200, &Page::<Schedule>::last(vec![]));
    let client = fake.client();

    let page = client
        .schedules()
        .list(
            &ListOptions::new()
                .after(Cursor::from_name("hourly"))
                .limit(1),
        )
        .expect("page");
    assert!(page.has_more());
    let all: Vec<_> = client.schedules().list_all().collect();
    assert_eq!(all.len(), 1);

    let seen = fake.requests();
    assert_eq!(
        seen[0].query_pairs(),
        [
            ("cursor", Cursor::from_name("hourly").to_string().as_str()),
            ("limit", "1")
        ]
    );
    assert!(seen[1].query.is_empty());
    assert_eq!(
        seen[2].query_pairs(),
        [("cursor", cursor.to_string().as_str())]
    );
}

// ---- calendars ----------------------------------------------------------

#[test]
fn calendar_crud_encodes_names_in_the_path() {
    let fake = Fake::start();
    let mut spec = CalendarSpec::new("business hours");
    spec.timezone = "Europe/Berlin".into();
    fake.reply_json(200, &spec);
    fake.reply_json(200, &spec);
    fake.reply_json(200, &Page::last(vec![spec.clone()]));
    fake.reply_status(204);
    let client = fake.client();

    assert_eq!(client.calendars().put(&spec).expect("stored"), spec);
    assert_eq!(client.calendars().get("business hours").expect("got"), spec);
    assert_eq!(
        client
            .calendars()
            .list(&ListOptions::new())
            .expect("list")
            .items,
        [spec.clone()]
    );
    client
        .calendars()
        .delete("business hours")
        .expect("deleted");

    let seen = fake.requests();
    let calls: Vec<String> = seen.iter().map(|s| s.call()).collect();
    assert_eq!(
        calls,
        [
            "PUT /v1/calendars/:name",
            "GET /v1/calendars/:name",
            "GET /v1/calendars",
            "DELETE /v1/calendars/:name",
        ]
    );
    assert_eq!(seen[0].path, "/v1/calendars/business%20hours");
    assert_eq!(seen[0].param("name"), Some("business hours"));
    assert_eq!(seen[0].json(), spec.to_json());
}

// ---- workflows ----------------------------------------------------------

#[test]
fn workflow_submit_get_list_and_cancel() {
    let fake = Fake::start();
    let spec = WorkflowSpec::new("etl")
        .step(StepSpec::new("fetch", JobSpec::new("etl", "fetch")))
        .step(StepSpec::new("store", JobSpec::new("etl", "store")).after("fetch"));
    let run = Workflow::new(workflow_id(), spec.clone(), t0());
    fake.reply_json(201, &run);
    fake.reply_json(200, &run);
    fake.reply_json(200, &Page::last(vec![run.clone()]));
    fake.reply_status(204);
    let client = fake.client();

    assert_eq!(client.workflows().submit(&spec).expect("submitted"), run);
    assert_eq!(client.workflows().get(workflow_id()).expect("got"), run);
    let page = client
        .workflows()
        .list(&ListOptions::new().limit(5))
        .expect("list");
    assert_eq!(page.items.len(), 1);
    client.workflows().cancel(workflow_id()).expect("cancelled");

    let seen = fake.requests();
    let calls: Vec<String> = seen.iter().map(|s| s.call()).collect();
    assert_eq!(
        calls,
        [
            "POST /v1/workflows",
            "GET /v1/workflows/:id",
            "GET /v1/workflows",
            "POST /v1/workflows/:id/cancel",
        ]
    );
    assert_eq!(seen[0].json(), spec.to_json());
    assert_eq!(seen[3].param("id"), Some("01HQ3K9Z8N6V2T4R5S7W0X1Y2Z"));
}
