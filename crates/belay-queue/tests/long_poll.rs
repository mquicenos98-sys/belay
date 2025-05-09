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

#[test]
fn a_waiting_lease_does_not_hold_the_engine_lock() {
    let h = Harness::new();
    let poller = poll(&h.engine, "q");
    until_waiting(&h.engine, 1);
    // Every one of these needs the engine lock; they complete while the
    // poller is parked.
    let started = Instant::now();
    for i in 0..20 {
        h.enqueue(JobSpec::new("other", format!("k{i}")));
    }
    assert_eq!(h.engine.list_queues().len(), 1);
    h.tick();
    assert!(started.elapsed() < PATIENCE);
    h.engine.shutdown();
    let (result, _) = poller.join().expect("poller thread");
    assert_eq!(result.expect("lease"), 0);
}

#[test]
fn shutdown_releases_every_poller_and_refuses_new_leases() {
    let h = Harness::new();
    let pollers: Vec<_> = (0..3).map(|_| poll(&h.engine, "q")).collect();
    until_waiting(&h.engine, 3);
    h.engine.shutdown();
    for poller in pollers {
        let (result, elapsed) = poller.join().expect("poller thread");
        assert_eq!(result.expect("lease"), 0);
        assert!(elapsed < PATIENCE);
    }
    assert!(h.engine.is_shutting_down());
    assert!(matches!(
        h.engine.lease(&waiting("q", LONG)),
        Err(Error::ShuttingDown)
    ));
    // Workers can still report what they hold.
    h.enqueue(JobSpec::new("q", "k"));
}

#[test]
fn a_rate_limited_poller_wakes_for_the_next_token() {
    // The token arrives by the real clock here, so this uses the system
    // clock rather than the harness's manual one.
    quiet_logs();
    let engine = QueueEngine::open(
        Arc::new(belay_store::MemKv::new()),
        belay_common::system_clock(),
        belay_proto::source::NoopSink::shared(),
        EngineOptions::new().max_poll_interval(Duration::from_secs(5)),
    )
    .expect("open");
    let mut config = QueueConfig::default();
    config.rate_limit = Some(belay_proto::RateLimit::new(20.0, 1));
    engine.set_config(&q("q"), config).expect("config");
    for i in 0..2 {
        engine
            .enqueue(JobSpec::new("q", format!("k{i}")))
            .expect("enqueue");
    }
    assert_eq!(engine.lease(&waiting("q", LONG)).expect("lease").len(), 1);
    let started = Instant::now();
    let jobs = engine.lease(&waiting("q", LONG)).expect("lease");
    let elapsed = started.elapsed();
    assert_eq!(jobs.len(), 1);
    // One token per 50ms; the poll interval alone would have taken 5s.
    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
}

#[test]
fn concurrent_pollers_split_the_jobs_without_duplicates() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let h = Harness::new();
    let threads = 6;
    let total = 150;
    let done = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(threads + 1));
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let engine = Arc::clone(&h.engine);
            let barrier = Arc::clone(&barrier);
            let done = Arc::clone(&done);
            thread::spawn(move || {
                barrier.wait();
                let mut ids = Vec::new();
                let deadline = Instant::now() + LONG;
                while done.load(Ordering::SeqCst) < total && Instant::now() < deadline {
                    let req = LeaseRequest::new(w("pool"), vec![q("q")])
                        .max_jobs(3)
                        .wait(Duration::from_millis(20));
                    for l in engine.lease(&req).expect("lease") {
                        engine
                            .complete(&l.job.id, &CompleteRequest::new(l.token))
                            .expect("complete");
                        ids.push(l.job.id);
                        done.fetch_add(1, Ordering::SeqCst);
                    }
                }
                ids
            })
        })
        .collect();
    barrier.wait();
    for i in 0..total {
        h.enqueue(JobSpec::new("q", format!("k{i}")));
    }
    let mut all: Vec<_> = handles
        .into_iter()
        .flat_map(|t| t.join().expect("worker thread"))
        .collect();
    assert_eq!(all.len(), total);
    all.sort();
    all.dedup();
    assert_eq!(all.len(), total, "a job was handed out twice");
    let info = h.engine.queue_info(&q("q")).expect("info");
    assert_eq!(info.counts.succeeded as usize, total);
    h.assert_consistent();
}
