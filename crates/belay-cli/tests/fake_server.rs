//! Commands against a fake server: the requests they send, how they print
//! answers in each output mode, and how failures map to exit statuses.

mod common;

use std::net::TcpListener;
use std::time::Duration;

use belay_cli::Exit;
use belay_json::json;
use belay_proto::api::{
    ApiError, CheckpointResponse, Cursor, EnqueueResponse, EnqueueResult, EnqueueStatus, Page,
    PreviewResponse, PurgeResponse, ServerInfo, TriggerResponse,
};
use belay_proto::{
    Job, JobError, JobSpec, JobState, QueueInfo, QueueName, RateLimit, Schedule, ScheduleSpec,
};
use common::{job_id, run_with, runner, sample_job, t0, Fake};

fn enqueued(n: u8, status: EnqueueStatus, state: JobState) -> EnqueueResponse {
    EnqueueResponse {
        results: vec![EnqueueResult {
            id: job_id(n),
            status,
            state,
        }],
    }
}

fn created(n: u8, state: JobState) -> EnqueueResponse {
    enqueued(n, EnqueueStatus::Created, state)
}

// ---- enqueue ----------------------------------------------------------------

#[test]
fn enqueue_sends_the_spec_built_from_flags() {
    let fake = Fake::start();
    fake.reply_json(200, &created(1, JobState::Scheduled));
    let out = fake
        .belay(
            r#"enqueue emails send_welcome --payload '{"user": 42}' -p 5 --delay 10m
               --max-attempts 3 --retry exp:1s*2..1m --timeout 30s
               --unique welcome:42:for:1h --on-conflict reject
               -t vip --tag new --tenant acme -m source=signup"#,
        )
        .ok();
    assert_eq!(
        out.stdout,
        format!("enqueued {} on emails (scheduled)\n", job_id(1))
    );
    let seen = fake.only();
    assert_eq!(seen.call(), "POST /v1/jobs");
    assert_eq!(
        seen.json(),
        json!({
            "queue": "emails",
            "kind": "send_welcome",
            "payload": {"user": 42},
            "priority": 5,
            "delay": "10m",
            "max_attempts": 3,
            "retry": "exp:1s*2..1m",
            "timeout": "30s",
            "unique": {"key": "welcome:42", "scope": "for:1h", "on_conflict": "reject"},
            "tags": ["vip", "new"],
            "tenant": "acme",
            "meta": {"source": "signup"}
        })
    );
}

#[test]
fn enqueue_output_modes_and_token() {
    let fake = Fake::start();
    for _ in 0..3 {
        fake.reply_json(200, &created(2, JobState::Ready));
    }
    assert_eq!(
        fake.belay("enqueue q k -q").ok().stdout,
        format!("{}\n", job_id(2))
    );
    let json = fake.belay("enqueue q k --json").ok().json();
    assert_eq!(json["id"], job_id(2).to_string());
    assert_eq!(json["status"], "created");
    let with_token = fake.runner().env("BELAY_TOKEN", "s3cret");
    assert!(run_with(&with_token, "enqueue q k")
        .ok()
        .stdout
        .starts_with("enqueued"));
    let seen = fake.requests();
    assert_eq!(seen[0].authorization, None);
    assert_eq!(seen[2].authorization.as_deref(), Some("Bearer s3cret"));
}

#[test]
fn enqueue_reports_duplicates() {
    let fake = Fake::start();
    fake.reply_json(
        200,
        &enqueued(3, EnqueueStatus::Duplicate, JobState::Running),
    );
    let out = fake.belay("enqueue q k --unique k1").ok();
    assert_eq!(
        out.stdout,
        format!(
            "job {} already holds the unique key; nothing enqueued (running)\n",
            job_id(3)
        )
    );
}

#[test]
fn enqueue_reads_the_payload_from_stdin_and_files() {
    let fake = Fake::start();
    fake.reply_json(200, &created(4, JobState::Ready));
    fake.reply_json(200, &created(5, JobState::Ready));
    let stdin = fake.runner().stdin(br#"{"from": "stdin"}"#.to_vec());
    run_with(&stdin, "enqueue q k --payload -").ok();
    let dir = belay_common::fs::TempDir::new().unwrap();
    let file = dir.join("payload.json");
    std::fs::write(&file, "[1, 2, 3]").unwrap();
    fake.belay(&format!("enqueue q k -d '@{}'", file.display()))
        .ok();
    let seen = fake.requests();
    assert_eq!(seen[0].json()["payload"], json!({"from": "stdin"}));
    assert_eq!(seen[1].json()["payload"], json!([1, 2, 3]));

    let out = fake
        .belay("enqueue q k -d @missing.json")
        .exits(Exit::Failure);
    assert!(
        out.stderr.starts_with("error: reading missing.json: "),
        "{}",
        out.stderr
    );
}

#[test]
fn enqueue_wait_polls_until_the_job_finishes() {
    let fake = Fake::start();
    fake.reply_json(200, &created(6, JobState::Ready));
    let mut running = sample_job(6);
    running.state = JobState::Running;
    fake.reply_json(200, &running);
    let mut done = sample_job(6);
    done.state = JobState::Succeeded;
    done.attempt = 1;
    done.started_at = Some(t0());
    done.finished_at = Some(t0() + Duration::from_millis(1500));
    done.result = Some(json!({"sent": true}));
    fake.reply_json(200, &done);
    let out = fake.belay("enqueue q k --wait").ok();
    assert_eq!(
        out.stdout,
        format!(
            "job {} succeeded after 1 attempt in 1.5s\n{{\n  \"sent\": true\n}}\n",
            job_id(6)
        )
    );
    assert_eq!(
        fake.calls(),
        ["POST /v1/jobs", "GET /v1/jobs/:id", "GET /v1/jobs/:id"]
    );
}

#[test]
fn waiting_on_a_job_that_dies_fails_with_its_error() {
    let fake = Fake::start();
    let mut dead = sample_job(7);
    dead.state = JobState::Dead;
    dead.attempt = 3;
    dead.record_error(JobError::new(t0(), 3, "smtp timeout"));
    fake.reply_json(200, &dead);
    let out = fake
        .belay(&format!("job wait {}", job_id(7)))
        .exits(Exit::Failure);
    assert!(
        out.stdout.contains("dead after 3 attempts"),
        "{}",
        out.stdout
    );
    let expected = format!("error: job {} is dead: smtp timeout\n", job_id(7));
    assert!(out.stderr.starts_with(&expected), "{}", out.stderr);
    assert!(out
        .stderr
        .contains("hint: see its errors with `belay job get"));
}

#[test]
fn waiting_gives_up_after_the_timeout() {
    let fake = Fake::start();
    for _ in 0..20 {
        fake.reply_json(200, &sample_job(8));
    }
    let out = fake
        .belay(&format!("job wait {} --timeout 60ms", job_id(8)))
        .exits(Exit::Failure);
    assert!(
        out.stderr.contains("is still ready after 60ms"),
        "{}",
        out.stderr
    );
}

// ---- job --------------------------------------------------------------------

#[test]
fn job_list_passes_filters_as_query_parameters() {
    let fake = Fake::start();
    fake.reply_json(200, &Page::<Job>::last(Vec::new()));
    let out = fake
        .belay(
            "job list --queue emails -s dead,retrying --kind send \
             --created-after 2025-03-01T00:00:00Z --order asc -n 50",
        )
        .ok();
    assert!(out.stdout.is_empty());
    assert_eq!(out.stderr, "no jobs match\n");
    assert_eq!(
        fake.only().query_pairs(),
        [
            ("queue", "emails"),
            ("state", "dead,retrying"),
            ("kind", "send"),
            ("created_after", "2025-03-01T00:00:00.000Z"),
            ("order", "asc"),
            ("limit", "50"),
        ]
    );
}

#[test]
fn job_list_table_json_and_quiet() {
    let fake = Fake::start();
    let mut dead = sample_job(2);
    dead.state = JobState::Dead;
    dead.attempt = 25;
    dead.record_error(JobError::new(t0(), 25, "boom"));
    let page = Page::new(
        vec![sample_job(1), dead],
        Some(Cursor::after_job(job_id(2))),
    );
    for _ in 0..3 {
        fake.reply_json(200, &page);
    }
    let out = fake.belay("job list").ok();
    assert_eq!(
        out.stdout,
        format!(
            "ID                          QUEUE   KIND  STATE  TRIES  AGE     INFO\n\
             {}  emails  send  ready      0  1m ago\n\
             {}  emails  send  dead      25  1m ago  boom\n",
            job_id(1),
            job_id(2)
        )
    );
    assert!(
        out.stderr
            .starts_with("showing 2 jobs; continue with --cursor "),
        "{}",
        out.stderr
    );

    let json = fake.belay("job list --json").ok().json();
    assert_eq!(json["items"].as_array().map(Vec::len), Some(2));
    assert!(json["next_cursor"].is_string());

    let quiet = fake.belay("job ls -q").ok();
    assert_eq!(quiet.stdout, format!("{}\n{}\n", job_id(1), job_id(2)));
    assert!(quiet.stderr.is_empty());
}

#[test]
fn job_list_all_follows_cursors() {
    let fake = Fake::start();
    let first = Page::new(vec![sample_job(1)], Some(Cursor::after_job(job_id(1))));
    fake.reply_json(200, &first);
    fake.reply_json(200, &Page::last(vec![sample_job(2)]));
    let out = fake.belay("job list --all -q").ok();
    assert_eq!(out.stdout, format!("{}\n{}\n", job_id(1), job_id(2)));
    let seen = fake.requests();
    assert_eq!(seen.len(), 2);
    assert!(seen[1].query.iter().any(|(k, _)| k == "cursor"));
    fake.belay("job list --all --cursor abc").exits(Exit::Usage);
}
