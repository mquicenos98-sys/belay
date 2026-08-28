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
