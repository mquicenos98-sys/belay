//! The queue engine embedded in a program, without a server: a durable
//! store in a temporary directory, a job enqueued, leased and completed,
//! and the result read back after the store is reopened.
//!
//! ```text
//! cargo run -p belay-queue --example embedded
//! ```

use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use belay_common::fs::TempDir;
use belay_common::system_clock;
use belay_json::json;
use belay_proto::api::{CompleteRequest, LeaseRequest};
use belay_proto::source::NoopSink;
use belay_proto::{JobId, JobSpec, JobState, QueueName, WorkerId};
use belay_queue::{EngineOptions, QueueEngine};
use belay_store::{Kv, LogKv, Options, SyncPolicy};

/// Opens the store in `dir` and the queue engine on top of it.
fn open(dir: &TempDir) -> Result<(Arc<LogKv>, QueueEngine), Box<dyn Error>> {
    // Every acknowledged change is synced before the call returns.
    let kv = Arc::new(LogKv::open(
        dir.path(),
        Options::new().sync(SyncPolicy::Always),
    )?);
    let store: Arc<dyn Kv> = kv.clone();
    // NoopSink drops the engine's events; belay-core feeds them to its
    // event bus instead.
    let queue = QueueEngine::open(
        store,
        system_clock(),
        Arc::new(NoopSink),
        EngineOptions::new(),
    )?;
    Ok((kv, queue))
}

/// Enqueues a job, then plays the worker: lease it, "send" the mail and
/// report the result with the lease token.
fn produce_and_work(dir: &TempDir) -> Result<JobId, Box<dyn Error>> {
    let (kv, queue) = open(dir)?;

    let created = queue.enqueue(
        JobSpec::new("emails", "send_welcome")
            .payload(json!({"user": 42, "template": "welcome-v2"}))
            .max_attempts(5)
            .tag("signup"),
    )?;
    println!("enqueued {} ({})", created.id, created.state);

    let request = LeaseRequest::new(
        WorkerId::new("example-worker")?,
        vec![QueueName::new("emails")?],
    )
    .lease_for(Duration::from_secs(30));
    let leased = queue.lease(&request)?;
    let Some(first) = leased.first() else {
        return Err("the job was not handed out".into());
    };
    let job = &first.job;
    println!(
        "leased {} ({}, attempt {}) with token {}",
        job.id, job.spec.kind, job.attempt, first.token
    );

    let user = job.spec.payload["user"].as_u64().unwrap_or_default();
    let result = json!({"sent_to": user, "provider": "smtp"});
    queue.complete(
        &job.id,
        &CompleteRequest {
            token: first.token,
            result: Some(result),
        },
    )?;
    println!("completed {}", job.id);

    // Closing syncs the log and releases the directory lock, so the
    // store can be opened again.
    queue.shutdown();
    kv.close()?;
    Ok(created.id)
}

fn main() -> Result<(), Box<dyn Error>> {
    let dir = TempDir::with_prefix("belay-embedded")?;
    let id = produce_and_work(&dir)?;

    // A new process would see the same thing: reopening replays the log.
    let (kv, queue) = open(&dir)?;
    let job = queue.get(&id)?;
    println!(
        "after reopening: job {} is {} with result {}",
        job.id,
        job.state,
        job.result
            .map_or_else(|| "none".to_owned(), |r| r.to_string()),
    );
    assert_eq!(job.state, JobState::Succeeded);
    queue.shutdown();
    kv.close()?;
    Ok(())
}
