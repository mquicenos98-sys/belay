//! Stopping a worker: draining running jobs, handing back the rest, and
//! the ways to start and stop one.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use belay_common::log::CaptureLogger;
use belay_common::Shutdown;
use belay_proto::api::FailRequest;
use belay_worker::builtin::EchoHandler;
use belay_worker::testing::{JobOutcome, MemorySource};
use belay_worker::{
    failure_kind, handler_fn, CancelReason, HandlerError, JobContext, KindOptions, Outcome,
    Registry, Worker,
};

use support::{config, config_logging_to, eventually, spec, start, Gate, PATIENCE};

fn handed_back(source: &MemorySource, id: &belay_proto::JobId) -> FailRequest {
    source
        .outcome(id)
        .and_then(|o| o.failure().cloned())
        .expect("job was failed back")
}

fn assert_is_shutdown_failure(req: &FailRequest) {
    assert_eq!(req.error, "worker shutting down");
    assert_eq!(req.kind.as_deref(), Some(failure_kind::SHUTDOWN));
    assert!(req.retryable);
    assert_eq!(req.retry_in, Some(Duration::ZERO));
}

#[test]
fn graceful_shutdown_lets_running_jobs_finish() {
    let source = Arc::new(MemorySource::new());
    let id = source.push(spec("work"));
    let gate = Gate::new();
    let saw_shutdown = Arc::new(AtomicBool::new(false));
    let (g, saw) = (Arc::clone(&gate), Arc::clone(&saw_shutdown));
    let registry = Registry::new().register(
        "work",
        handler_fn(move |ctx: &mut JobContext| {
            g.pass();
            saw.store(ctx.is_shutting_down(), Ordering::SeqCst);
            Ok(Outcome::with("finished"))
        }),
    );
    let worker = start(&source, registry, config(1));

    assert!(eventually(|| gate.entered() == 1));
    worker.shutdown();
    // Heartbeats keep the lease alive while the job drains.
    let beats = source.view(|v| v.heartbeats_for(&id));
    assert!(source.wait_until(PATIENCE, |v| v.heartbeats_for(&id) > beats));
    gate.open();
    let report = worker.join();

    assert!(report.drained);
    assert_eq!(report.detached_threads, 0);
    assert_eq!(
        source.outcome(&id),
        Some(JobOutcome::Completed(Some("finished".into())))
    );
    assert!(
        saw_shutdown.load(Ordering::SeqCst),
        "handlers can see the shutdown"
    );
    assert_eq!((report.stats.succeeded, report.stats.handed_back), (1, 0));
}

#[test]
fn jobs_running_past_the_grace_period_are_handed_back() {
    let source = Arc::new(MemorySource::new());
    source.set_requeue(false);
    let id = source.push(spec("stubborn"));
    let gate = Gate::new();
    let g = Arc::clone(&gate);
    let registry = Registry::new().register(
        "stubborn",
        handler_fn(move |_: &mut JobContext| {
            g.pass();
            Ok(Outcome::done())
        }),
    );
    let config = config(1)
        .shutdown_grace(Duration::from_millis(30))
        .cancel_grace(Duration::from_millis(30));
    let worker = start(&source, registry, config);

    assert!(eventually(|| gate.entered() == 1));
    let report = worker.stop();
    assert!(!report.drained);
    assert_eq!(
        report.detached_threads, 1,
        "the stuck thread cannot be joined"
    );
    assert_is_shutdown_failure(&handed_back(&source, &id));
    let stats = &report.stats;
    assert_eq!(
        (stats.handed_back, stats.abandoned, stats.retried),
        (1, 1, 1)
    );
    assert_eq!((stats.running, stats.busy_slots), (0, 1));
    assert!(
        report
            .to_string()
            .contains("not drained, 1 handler threads detached"),
        "{report}"
    );
    gate.open();
}

#[test]
fn cooperative_handlers_are_cancelled_when_the_grace_period_ends() {
    let source = Arc::new(MemorySource::new());
    let id = source.push(spec("long"));
    let reason = Arc::new(Mutex::new(None));
    let started = Arc::new(AtomicBool::new(false));
    let (r, s) = (Arc::clone(&reason), Arc::clone(&started));
    let registry = Registry::new().register(
        "long",
        handler_fn(move |ctx: &mut JobContext| {
            s.store(true, Ordering::SeqCst);
            let finished = ctx.sleep_or_cancel(PATIENCE);
            *r.lock().unwrap() = ctx.cancel_reason();
            if finished {
                Ok(Outcome::done())
            } else {
                Err(HandlerError::cancelled())
            }
        }),
    );
    let worker = start(
        &source,
        registry,
        config(1).shutdown_grace(Duration::from_millis(20)),
    );

    assert!(eventually(|| started.load(Ordering::SeqCst)));
    let report = worker.stop();
    assert!(!report.drained);
    assert_eq!(
        report.detached_threads, 0,
        "the handler returned when cancelled"
    );
    assert_eq!(*reason.lock().unwrap(), Some(CancelReason::ShuttingDown));
    assert_is_shutdown_failure(&handed_back(&source, &id));
    assert_eq!(
        source.view(|v| v.outcomes_for(&id).len()),
        1,
        "reported once"
    );
}

#[test]
fn jobs_waiting_for_their_kind_are_handed_back_at_once() {
    let source = Arc::new(MemorySource::new());
    let running = source.push(spec("serial"));
    let waiting = source.push(spec("serial"));
    let gate = Gate::new();
    let g = Arc::clone(&gate);
    // With an unlimited kind registered too, the worker leases for its free
    // slots and the second serial job has to wait here.
    let registry = Registry::new()
        .register_with(
            "serial",
            handler_fn(move |_: &mut JobContext| {
                g.pass();
                Ok(Outcome::done())
            }),
            KindOptions::new().concurrency(1),
        )
        .register("echo", EchoHandler);
    let worker = start(&source, registry, config(2));

    assert!(eventually(
        || gate.entered() == 1 && worker.stats().waiting == 1
    ));
    worker.shutdown();
    // Handed back while the other job is still held by the gate.
    assert!(source.wait_until(PATIENCE, |v| v.outcome(&waiting).is_some()));
    assert!(source.outcome(&running).is_none());
    assert_is_shutdown_failure(&handed_back(&source, &waiting));

    gate.open();
    let report = worker.join();
    assert!(report.drained);
    assert_eq!(source.outcome(&running), Some(JobOutcome::Completed(None)));
    assert_eq!((report.stats.handed_back, report.stats.abandoned), (1, 0));
    assert_eq!(gate.entered(), 1, "the waiting job never started");
}

#[test]
fn jobs_from_a_lease_in_flight_at_shutdown_are_handed_back() {
    let source = Arc::new(MemorySource::new());
    let capture = CaptureLogger::new();
    let config = config_logging_to(1, capture.clone()).poll_wait(Duration::from_secs(30));
    let worker = start(
        &source,
        Registry::new().register("echo", EchoHandler),
        config,
    );

    // The poller is now inside a long poll.
    assert!(source.wait_until(PATIENCE, |v| v.lease_calls() == 1));
    worker.shutdown();
    assert!(eventually(|| capture.contains("worker shutting down")));
    let id = source.push(spec("echo"));

    let report = worker.join();
    assert_is_shutdown_failure(&handed_back(&source, &id));
    assert_eq!(
        (
            report.stats.leased,
            report.stats.handed_back,
            report.stats.succeeded
        ),
        (1, 1, 0)
    );
}

#[test]
fn idle_workers_stop_promptly() {
    let source = Arc::new(MemorySource::new());
    let worker = start(
        &source,
        Registry::new().register("echo", EchoHandler),
        config(4),
    );
    assert!(source.wait_until(PATIENCE, |v| v.lease_calls() >= 1));
    let started = Instant::now();
    let report = worker.stop();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(report.drained);
    assert_eq!(report.detached_threads, 0);
    assert_eq!(report.stats.leased, 0);
    assert_eq!(report.stats.concurrency, 4);
}

#[test]
fn run_blocks_until_the_shutdown_signal() {
    let source = Arc::new(MemorySource::new());
    let id = source.push(spec("echo").payload(belay_json::json!(7)));
    let worker = Worker::new(
        source.clone(),
        Registry::new().register("echo", EchoHandler),
        config(1),
    )
    .expect("valid setup");
    let stats = worker.stats_handle();
    let shutdown = Shutdown::new();
    let signal = shutdown.clone();
    let runner = thread::spawn(move || worker.run(signal));

    assert!(source.wait_until(PATIENCE, |v| v.outcome(&id).is_some()));
    assert_eq!(stats.snapshot().succeeded, 1);
    shutdown.trigger();
    let report = runner
        .join()
        .expect("run does not panic")
        .expect("threads start");
    assert_eq!(report.stats.succeeded, 1);
    assert!(report.uptime > Duration::ZERO);
}

#[test]
fn dropping_the_handle_shuts_the_worker_down() {
    let source = Arc::new(MemorySource::new());
    let capture = CaptureLogger::new();
    let worker = start(
        &source,
        Registry::new().register("echo", EchoHandler),
        config_logging_to(1, capture.clone()),
    );
    let stats = worker.stats_handle();
    assert!(!worker.is_finished());
    drop(worker);
    assert!(eventually(|| capture.contains("worker stopped")));
    assert_eq!(stats.snapshot().running, 0);
}

#[test]
fn invalid_setups_are_rejected_before_anything_starts() {
    let source: Arc<MemorySource> = Arc::new(MemorySource::new());
    let err = Worker::new(source.clone(), Registry::new(), config(1)).unwrap_err();
    assert_eq!(err.to_string(), "no job handlers registered");
    let err = Worker::new(
        source.clone(),
        Registry::new().register("echo", EchoHandler),
        config(0),
    )
    .unwrap_err();
    assert!(err
        .to_string()
        .contains("concurrency must be between 1 and 1024"));
    assert_eq!(source.view(|v| v.lease_calls()), 0);
}
