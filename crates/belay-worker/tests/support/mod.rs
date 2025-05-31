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

impl Gate {
    pub fn new() -> Arc<Gate> {
        Arc::new(Gate::default())
    }

    /// Blocks until the gate opens (at most `PATIENCE`, so a failing test
    /// cannot hang the suite).
    pub fn pass(&self) {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let deadline = Instant::now() + PATIENCE;
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        while !*open {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            open = self
                .cv
                .wait_timeout(open, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    pub fn open(&self) {
        *self.open.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.cv.notify_all();
    }

    pub fn entered(&self) -> usize {
        self.entered.load(Ordering::SeqCst)
    }
}

/// Tracks how many handlers run at once.
#[derive(Default)]
pub struct Concurrency {
    now: AtomicUsize,
    max: AtomicUsize,
}

impl Concurrency {
    pub fn new() -> Arc<Concurrency> {
        Arc::new(Concurrency::default())
    }

    pub fn enter(&self) {
        let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
        self.max.fetch_max(now, Ordering::SeqCst);
    }

    pub fn leave(&self) {
        self.now.fetch_sub(1, Ordering::SeqCst);
    }

    pub fn max(&self) -> usize {
        self.max.load(Ordering::SeqCst)
    }
}
