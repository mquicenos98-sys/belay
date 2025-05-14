//! Shared test harness: an engine on a manual clock with a collecting
//! event sink, plus shorthands for the worker protocol.
// Each test binary uses a different subset of these helpers.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use belay_common::log::{set_max_level, CaptureLogger, LevelFilter};
use belay_common::{ManualClock, Timestamp};
use belay_proto::api::{
    CompleteRequest, FailRequest, HeartbeatRequest, HeartbeatResponse, JobFilter, LeaseRequest,
    LeasedJob,
};
use belay_proto::source::CollectingSink;
use belay_proto::{Job, JobId, JobSpec, JobState, QueueName, WorkerId};
use belay_queue::{EngineOptions, QueueEngine, TickReport};
use belay_store::vfs::MemFs;
use belay_store::{
    CheckpointInfo, KeyRange, Kv, KvStats, LogKv, MemKv, Options, StoreError, SyncPolicy,
    WriteBatch,
};

/// Where every test clock starts: 2023-11-14T22:13:20Z.
pub fn start() -> Timestamp {
    Timestamp::from_secs(1_700_000_000)
}

pub fn q(name: &'static str) -> QueueName {
    QueueName::from_static(name)
}

pub fn w(name: &'static str) -> WorkerId {
    WorkerId::from_static(name)
}

/// Keeps engine and store log lines out of the test output.
pub fn quiet_logs() {
    set_max_level(LevelFilter::Off);
}

/// A durable store on an in-memory file system that syncs every write.
pub fn durable(fs: &MemFs) -> Arc<dyn Kv> {
    quiet_logs();
    let opts = Options::new()
        .fs(fs.clone())
        .sync(SyncPolicy::Always)
        .manual_checkpoints();
    Arc::new(LogKv::open(PathBuf::from("store"), opts).expect("opening the store"))
}

/// Drops a store whose file system crashed under it, keeping the expected
/// "closing store failed" warning out of the output.
pub fn drop_quietly<T>(value: T) {
    CaptureLogger::new().scope(move || drop(value));
}
