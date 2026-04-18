//! End to end: commands against a real belay server (belay-server over an
//! in-memory engine), plus `belay dev` and `belay server` themselves.

mod common;

use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use belay_cli::{Exit, Runner};
use belay_common::fs::TempDir;
use belay_proto::{QueueName, WorkerId};
use belay_worker::{builtin, WorkerConfig, WorkerHandle};
use common::{belay, eventually, run_with, words, Live};

/// A worker inside the test's engine, for commands that wait on jobs.
fn embedded_worker(live: &Live) -> WorkerHandle {
    let config = WorkerConfig::new(
        WorkerId::from_static("e2e-worker"),
        vec![QueueName::from_static("default")],
    )
    .concurrency(2)
    .poll_wait(Duration::from_millis(50));
    live.engine
        .embedded_worker(builtin::defaults(), config)
        .expect("worker starts")
}

#[test]
fn job_lifecycle() {
    let live = Live::start();
    let id = live
        .belay(r#"enqueue emails send -d '{"user": 42}' -t vip -q"#)
        .ok()
        .id();
    let job = live.belay(&format!("job get {id} --json")).ok().json();
    assert_eq!(job["state"], "ready");
    assert_eq!(job["spec"]["payload"]["user"], 42);
    let listed = live.belay("job list --queue emails --tag vip -q").ok();
    assert_eq!(listed.id(), id);

    // Only finished jobs can be deleted.
    let out = live.belay(&format!("job delete {id}")).exits(Exit::Failure);
    assert!(out.stderr.starts_with("error: "), "{}", out.stderr);

    let out = live.belay(&format!("job cancel {id}")).ok();
    assert_eq!(out.stdout, format!("cancelled {id} (now cancelled)\n"));
    let out = live.belay(&format!("job retry {id}")).ok();
    assert_eq!(out.stdout, format!("retried {id} (now ready)\n"));

    let later = live.belay("enqueue emails send --delay 1h -q").ok().id();
    let state = |id: &str| live.belay(&format!("job get {id} --json")).ok().json()["state"].clone();
    assert_eq!(state(&later), "scheduled");
    live.belay(&format!("job promote {later}")).ok();
    assert_eq!(state(&later), "ready");

    let out = live
        .belay("job get 01HQ3K9Z8N6V2T4R5S7W0X1Y2Z")
        .exits(Exit::NotFound);
    assert!(out.stderr.contains("not found"), "{}", out.stderr);
}

#[test]
fn unique_keys_deduplicate_enqueues() {
    let live = Live::start();
    let first = live.belay("enqueue q k --unique user:7 --json").ok().json();
    let second = live.belay("enqueue q k --unique user:7 --json").ok().json();
    assert_eq!(first["status"], "created");
    assert_eq!(second["status"], "duplicate");
    assert_eq!(first["id"], second["id"]);
    let out = live
        .belay("enqueue q k --unique user:7 --on-conflict reject")
        .exits(Exit::Failure);
    assert!(out.stderr.starts_with("error: "), "{}", out.stderr);
}

#[test]
fn enqueue_wait_returns_the_result_or_the_failure() {
    let live = Live::start();
    let worker = embedded_worker(&live);
    let out = live
        .belay(r#"enqueue default echo -d '{"hello": "world"}' --wait"#)
        .ok();
    assert!(
        out.stdout.contains(" succeeded after 1 attempt"),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("\"hello\": \"world\""),
        "{}",
        out.stdout
    );

    let out = live
        .belay("enqueue default fail --max-attempts 1 --wait --wait-timeout 20s")
        .exits(Exit::Failure);
    assert!(
        out.stdout.contains(" dead after 1 attempt"),
        "{}",
        out.stdout
    );
    assert!(
        out.stderr.contains("is dead: failed on purpose"),
        "{}",
        out.stderr
    );
    assert!(worker.stop().drained);
}

#[test]
fn queues_can_be_paused_configured_and_purged() {
    let live = Live::start();
    for _ in 0..3 {
        live.belay("enqueue emails send -q").ok();
    }
    let out = live.belay("queue list").ok();
    assert!(
        out.stdout.contains("\nemails  active      3"),
        "{}",
        out.stdout
    );

    live.belay("queue pause emails").ok();
    let info = live.belay("queue info emails --json").ok().json();
    assert_eq!(info["paused"], true);
    live.belay("queue resume emails -q").ok();

    let out = live
        .belay("queue config emails --set max_concurrency=2 --set default_timeout=90s")
        .ok();
    assert!(
        out.stdout.contains("max_concurrency = 2\n"),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("default_timeout = 1m30s\n"),
        "{}",
        out.stdout
    );
    let shown = live.belay("queue config emails").ok();
    assert!(
        shown
            .stdout
            .starts_with("max_concurrency = 2\nrate_limit = none\n"),
        "{}",
        shown.stdout
    );

    let out = live.belay("queue purge emails --yes").ok();
    assert_eq!(out.stdout, "purged 3 jobs from emails\n");
    live.belay("queue delete emails -y").ok();
}

#[test]
fn schedules_and_calendars() {
    let live = Live::start();
    let out = live
        .belay(
            "calendar put business --tz America/Bogota --window 'mon-fri 09:00-17:00' \
             --exclude-date 2025-12-25",
        )
        .ok();
    assert_eq!(out.stdout, "saved calendar business\n");
    let out = live.belay("calendar list").ok();
    assert!(
        out.stdout
            .contains("business  America/Bogota  mon,tue,wed,thu,fri 09:00-17:00  2025-12-25"),
        "{}",
        out.stdout
    );

    let standup = "schedule add standup --cron '30 9 * * MON-FRI' --tz America/Bogota \
                   --queue reports --kind build";
    let out = live
        .belay(&format!(
            "{standup} --calendar business --description 'standup notes'"
        ))
        .ok();
    assert!(
        out.stdout
            .starts_with("created schedule standup; next fire "),
        "{}",
        out.stdout
    );
    live.belay("schedule add standup --cron @daily --queue q --kind k")
        .exits(Exit::Failure);
    live.belay("schedule add standup --cron @daily --queue q --kind k --replace")
        .ok();
    live.belay(&format!("{standup} --replace")).ok();

    let get = live.belay("schedule get standup").ok();
    assert!(
        get.stdout.contains("(at 09:30 on Monday through Friday)"),
        "{}",
        get.stdout
    );
    let preview = live.belay("schedule preview standup -n 3").ok();
    assert_eq!(preview.stdout.lines().count(), 4, "{}", preview.stdout);
    assert!(
        preview.stdout.contains("T09:30:00-05:00"),
        "{}",
        preview.stdout
    );

    let job = live.belay("schedule trigger standup -q").ok().id();
    let fired = live.belay(&format!("job get {job} --json")).ok().json();
    assert_eq!(fired["spec"]["queue"], "reports");

    live.belay("schedule pause standup").ok();
    let listed = live.belay("schedule list").ok();
    assert!(
        listed
            .stdout
            .contains("standup  30 9 * * MON-FRI  America/Bogota  paused"),
        "{}",
        listed.stdout
    );
    live.belay("schedule resume standup").ok();
    live.belay("schedule rm standup").ok();
    live.belay("schedule get standup").exits(Exit::NotFound);
    live.belay("calendar rm business").ok();
    live.belay("calendar get business").exits(Exit::NotFound);
}

#[test]
fn workflows_run_to_completion() {
    let live = Live::start();
    let worker = embedded_worker(&live);
    let spec = r#"{
        "name": "etl",
        "steps": [
            {"name": "extract", "job": {"queue": "default", "kind": "echo", "payload": {"rows": 3}}},
            {"name": "load", "job": {"queue": "default", "kind": "echo"}, "depends_on": ["extract"]}
        ]
    }"#;
    let out = live
        .belay_stdin(spec, "workflow submit - --wait --wait-timeout 20s")
        .ok();
    for line in [
        "state     succeeded",
        "\nextract  succeeded",
        "\nload     succeeded",
    ] {
        assert!(
            out.stdout.contains(line),
            "{line:?} missing from\n{}",
            out.stdout
        );
    }
    let listed = live.belay("workflow list --json").ok().json();
    assert_eq!(listed.as_array().map(Vec::len), Some(1));

    let cyclic = r#"{"name": "loop", "steps": [
        {"name": "a", "job": {"queue": "q", "kind": "k"}, "depends_on": ["b"]},
        {"name": "b", "job": {"queue": "q", "kind": "k"}, "depends_on": ["a"]}]}"#;
    let out = live
        .belay_stdin(cyclic, "workflow submit -")
        .exits(Exit::Failure);
    assert!(out.stderr.contains("dependency cycle"), "{}", out.stderr);
    assert!(worker.stop().drained);
}

#[test]
fn a_burst_worker_drains_the_queue_over_http() {
    let live = Live::start();
    let ids: Vec<String> = (0..3)
        .map(|n| {
            live.belay(&format!("enqueue default echo -d '{{\"n\": {n}}}' -q"))
                .ok()
                .id()
        })
        .collect();
    let out = live
        .belay("worker --queues default --burst --poll-wait 200ms -c 2 -q")
        .ok();
    assert!(out.stdout.is_empty(), "{}", out.stdout);
    for id in &ids {
        let job = live.belay(&format!("job get {id} --json")).ok().json();
        assert_eq!(job["state"], "succeeded", "{job}");
    }
    let workers = live.belay("workers --json").ok().json();
    assert_eq!(workers[0]["succeeded"], 3);

    live.belay("enqueue default echo -q").ok();
    let summary = live
        .belay("worker --max-jobs 1 --poll-wait 100ms --json")
        .ok()
        .json();
    assert_eq!(summary["succeeded"], 1);
}

#[test]
fn events_tail_replays_and_filters() {
    let live = Live::start();
    live.belay("enqueue emails send -q").ok();
    live.belay("queue pause emails").ok();
    let out = live.belay("events tail --types queue.* -n 1").ok();
    let line = out.stdout.trim();
    assert!(line.contains("queue.paused"), "{line}");
    assert!(line.ends_with("queue=emails"), "{line}");
    let event = live
        .belay("events tail --types job.enqueued -n 1 --json")
        .ok()
        .json();
    assert_eq!(event["type"], "job.enqueued");
    assert_eq!(event["queue"], "emails");
    live.belay("events tail --types job.nope -n 1")
        .exits(Exit::Usage);
}

#[test]
fn system_and_admin_commands() {
    let live = Live::start();
    let info = live.belay("info").ok().stdout;
    let version = format!("version  {}", env!("CARGO_PKG_VERSION"));
    for line in [version.as_str(), "storage  memory", "ready    yes"] {
        assert!(info.contains(line), "{line:?} missing from\n{info}");
    }
    assert_eq!(live.belay("info --json").ok().json()["ready"], true);

    let members = live.belay("members").ok().stdout;
    assert!(
        members.contains("alive") && members.contains("yes"),
        "{members}"
    );

    live.belay("enqueue emails send -q").ok();
    let out = live.belay("admin verify").ok();
    assert!(
        out.stdout.starts_with("consistent: checked 1 job, 1 queue"),
        "{}",
        out.stdout
    );
    assert!(live.belay("admin store").ok().stdout.contains("storage"));
    assert_eq!(
        live.belay("admin store --json").ok().json()["storage"],
        "memory"
    );
    live.belay("admin checkpoint -q").ok();
}

#[test]
fn admin_shutdown_stops_the_server() {
    let mut live = Live::start();
    let out = live.belay("admin shutdown").ok();
    assert!(out.stdout.contains("is shutting down"), "{}", out.stdout);
    // Returns only once the HTTP server has drained and the engine stopped.
    let handle = live.handle.take().expect("running");
    handle.wait();
    assert!(live.engine.is_shutting_down());
}
