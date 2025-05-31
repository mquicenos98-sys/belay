//! Shared helpers for the worker integration tests: a fast configuration,
//! a gate that holds handlers until the test lets them go, and polling
//! for conditions that live on the worker side.

#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use belay_common::backoff::BackoffPolicy;
use belay_common::log::CaptureLogger;
use belay_proto::{JobSpec, QueueName, WorkerId};
use belay_worker::testing::MemorySource;
use belay_worker::{Registry, Worker, WorkerConfig, WorkerHandle};

/// Upper bound for anything a test waits for; generous so a loaded
/// machine does not cause flakes, never reached when things work.
pub const PATIENCE: Duration = Duration::from_secs(10);

/// Timings scaled down so each test takes milliseconds; log output goes
/// to a capture buffer instead of cluttering the test run.
pub fn config(concurrency: usize) -> WorkerConfig {
    config_logging_to(concurrency, CaptureLogger::new())
}

/// [`config`] with the worker's log records going to `capture`.
pub fn config_logging_to(concurrency: usize, capture: CaptureLogger) -> WorkerConfig {
    WorkerConfig::new(
        WorkerId::from_static("test-worker"),
        vec![QueueName::from_static("jobs")],
    )
    .concurrency(concurrency)
    .poll_wait(Duration::from_millis(20))
    .poll_interval(Duration::from_millis(5))
    .heartbeat_every(Duration::from_millis(10))
    .shutdown_grace(Duration::from_secs(5))
    .cancel_grace(Duration::from_millis(100))
    .source_backoff(BackoffPolicy::constant(Duration::from_millis(5)))
    .logger(Arc::new(capture))
}

/// A job of `kind` on the test queue.
pub fn spec(kind: &str) -> JobSpec {
    JobSpec::new("jobs", kind)
}

pub fn start(source: &Arc<MemorySource>, registry: Registry, config: WorkerConfig) -> WorkerHandle {
    Worker::new(source.clone(), registry, config)
        .expect("valid worker setup")
        .spawn()
        .expect("worker threads start")
}

/// Polls `cond` until it holds; for conditions the source cannot signal.
pub fn eventually(mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        thread::sleep(Duration::from_millis(2));
    }
    cond()
}

/// Holds handlers until opened, and counts how many arrived.
#[derive(Default)]
pub struct Gate {
    open: Mutex<bool>,
    cv: Condvar,
    entered: AtomicUsize,
}
