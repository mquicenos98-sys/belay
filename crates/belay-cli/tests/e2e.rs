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
