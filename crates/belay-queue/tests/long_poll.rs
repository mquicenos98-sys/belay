//! Long-polling leases: waits that end early when work arrives, end on
//! time when it does not, and never block the rest of the engine.
//!
//! These tests use real threads and real (short) waits; generous upper
//! bounds keep them reliable on a loaded machine.

mod support;

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use belay_proto::api::{CompleteRequest, LeaseRequest};
use belay_proto::{JobSpec, QueueConfig};
use belay_queue::{EngineOptions, Error, QueueEngine};
use support::*;

/// Long enough that finishing early proves a wakeup, short enough that a
/// failing test does not hang the suite.
const LONG: Duration = Duration::from_secs(10);

const PATIENCE: Duration = Duration::from_secs(5);

fn waiting(queue: &'static str, wait: Duration) -> LeaseRequest {
    LeaseRequest::new(w("poller"), vec![q(queue)]).wait(wait)
}

/// Starts a long-polling lease on another thread; returns its handle.
fn poll(
    engine: &Arc<QueueEngine>,
    queue: &'static str,
) -> thread::JoinHandle<(Result<usize, Error>, Duration)> {
    let engine = Arc::clone(engine);
    thread::spawn(move || {
        let started = Instant::now();
        let result = engine.lease(&waiting(queue, LONG)).map(|jobs| jobs.len());
        (result, started.elapsed())
    })
}

/// Waits until `n` lease requests are parked in the long poll.
fn until_waiting(engine: &QueueEngine, n: usize) {
    let deadline = Instant::now() + PATIENCE;
    while engine.stats().waiting_leases < n {
        assert!(Instant::now() < deadline, "poller never started waiting");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn a_job_already_available_returns_without_waiting() {
    let h = Harness::new();
    h.enqueue(JobSpec::new("q", "k"));
    let started = Instant::now();
    let jobs = h.engine.lease(&waiting("q", LONG)).expect("lease");
    assert_eq!(jobs.len(), 1);
    assert!(started.elapsed() < PATIENCE);
}

#[test]
fn an_empty_wait_times_out_with_nothing() {
    let h = Harness::new();
    let started = Instant::now();
    let jobs = h
        .engine
        .lease(&waiting("q", Duration::from_millis(30)))
        .expect("lease");
    let elapsed = started.elapsed();
    assert!(jobs.is_empty());
    assert!(elapsed >= Duration::from_millis(30), "{elapsed:?}");
    let counters = h.engine.stats().counters;
    assert_eq!((counters.lease_requests, counters.empty_leases), (1, 1));
}

#[test]
fn an_enqueue_on_another_thread_wakes_the_poller() {
    let h = Harness::new();
    let poller = poll(&h.engine, "q");
    until_waiting(&h.engine, 1);
    h.enqueue(JobSpec::new("q", "k"));
    let (result, elapsed) = poller.join().expect("poller thread");
    assert_eq!(result.expect("lease"), 1);
    assert!(elapsed < PATIENCE, "woke after {elapsed:?}");
}

#[test]
fn a_tick_promoting_a_timer_wakes_the_poller() {
    let h = Harness::new();
    h.enqueue(JobSpec::new("q", "k").delay(Duration::from_secs(60)));
    let poller = poll(&h.engine, "q");
    until_waiting(&h.engine, 1);
    h.secs(60);
    assert_eq!(h.tick().promoted, 1);
    let (result, elapsed) = poller.join().expect("poller thread");
    assert_eq!(result.expect("lease"), 1);
    assert!(elapsed < PATIENCE);
}

#[test]
fn resuming_a_paused_queue_wakes_the_poller() {
    let h = Harness::new();
    h.engine.pause(&q("q")).expect("pause");
    h.enqueue(JobSpec::new("q", "k"));
    let poller = poll(&h.engine, "q");
    until_waiting(&h.engine, 1);
    h.engine.resume(&q("q")).expect("resume");
    let (result, elapsed) = poller.join().expect("poller thread");
    assert_eq!(result.expect("lease"), 1);
    assert!(elapsed < PATIENCE);
}

#[test]
fn freed_concurrency_wakes_the_poller() {
    let h = Harness::new();
    let mut config = QueueConfig::default();
    config.max_concurrency = Some(1);
    h.engine.set_config(&q("q"), config).expect("config");
    h.enqueue(JobSpec::new("q", "a"));
    h.enqueue(JobSpec::new("q", "b"));
    let running = h.lease_one("q");
    let poller = poll(&h.engine, "q");
    until_waiting(&h.engine, 1);
    h.engine
        .complete(&running.job.id, &CompleteRequest::new(running.token))
        .expect("complete");
    let (result, elapsed) = poller.join().expect("poller thread");
    assert_eq!(result.expect("lease"), 1);
    assert!(elapsed < PATIENCE);
}

#[test]
fn work_in_another_queue_does_not_satisfy_the_poller() {
    let h = Harness::new();
    let started = Instant::now();
    let other = {
        let engine = Arc::clone(&h.engine);
        thread::spawn(move || {
            until_waiting(&engine, 1);
            engine.enqueue(JobSpec::new("other", "k")).expect("enqueue");
        })
    };
    let jobs = h
        .engine
        .lease(&waiting("q", Duration::from_millis(200)))
        .expect("lease");
    other.join().expect("enqueuer thread");
    assert!(jobs.is_empty());
    assert!(started.elapsed() >= Duration::from_millis(200));
}
