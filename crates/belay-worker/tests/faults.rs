//! Things going wrong around a job: lost leases, cancellation requests,
//! timeouts (cooperative and not), source outages, and failed heartbeats
//! and reports.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use belay_common::log::CaptureLogger;
use belay_proto::api::ApiError;
use belay_worker::builtin::EchoHandler;
use belay_worker::testing::{JobOutcome, MemorySource};
use belay_worker::{
    failure_kind, handler_fn, CancelReason, HandlerError, JobContext, KindOptions, Outcome,
    Registry,
};

use support::{config, config_logging_to, eventually, spec, start, Gate, PATIENCE};

/// What a cooperative handler saw.
#[derive(Default)]
struct Probe {
    started: AtomicBool,
    reason: Mutex<Option<CancelReason>>,
}

impl Probe {
    fn started(&self) -> bool {
        self.started.load(Ordering::SeqCst)
    }

    fn reason(&self) -> Option<CancelReason> {
        *self.reason.lock().unwrap()
    }
}

/// A handler that waits (cooperatively) until cancelled, records why,
/// and then returns `result`.
fn waits_for_cancel(
    probe: &Arc<Probe>,
    result: fn() -> Result<Outcome, HandlerError>,
) -> impl belay_worker::Handler {
    let probe = Arc::clone(probe);
    handler_fn(move |ctx: &mut JobContext| {
        probe.started.store(true, Ordering::SeqCst);
        let slept_through = ctx.sleep_or_cancel(PATIENCE);
        *probe.reason.lock().unwrap() = ctx.cancel_reason();
        if slept_through {
            return Err(HandlerError::fatal("never cancelled"));
        }
        result()
    })
}

#[test]
fn lost_leases_abandon_the_job_and_drop_its_result() {
    let source = Arc::new(MemorySource::new());
    let id = source.push(spec("long"));
    let probe = Arc::new(Probe::default());
    let registry = Registry::new().register(
        "long",
        waits_for_cancel(&probe, || Ok(Outcome::with("late"))),
    );
    let worker = start(&source, registry, config(1));

    assert!(eventually(|| probe.started()));
    source.revoke_lease(&id);
    assert!(eventually(|| worker.stats().abandoned == 1));
    assert!(eventually(|| worker.stats().busy_slots == 0));

    assert_eq!(probe.reason(), Some(CancelReason::LeaseLost));
    assert_eq!(
        source.outcome(&id),
        None,
        "nothing is reported for a lost lease"
    );
    let stats = worker.stop().stats;
    assert_eq!((stats.succeeded, stats.failed, stats.running), (0, 0, 0));
}

#[test]
fn a_lease_lost_while_reporting_drops_the_result() {
    let source = Arc::new(MemorySource::new());
    let id = source.push(spec("work"));
    let gate = Gate::new();
    let g = Arc::clone(&gate);
    let registry = Registry::new().register(
        "work",
        handler_fn(move |_: &mut JobContext| {
            g.pass();
            Ok(Outcome::done())
        }),
    );
    // No heartbeats during the test, so the loss shows up at report time.
    let worker = start(
        &source,
        registry,
        config(1).heartbeat_every(Duration::from_secs(20)),
    );

    assert!(eventually(|| gate.entered() == 1));
    source.revoke_lease(&id);
    gate.open();
    assert!(eventually(|| worker.stats().abandoned == 1));
    assert_eq!(source.outcome(&id), None);
    assert_eq!(worker.stop().stats.succeeded, 0);
}

#[test]
fn cancel_requests_reach_the_handler_and_are_reported() {
    let source = Arc::new(MemorySource::new());
    let id = source.push(spec("long"));
    let probe = Arc::new(Probe::default());
    let registry = Registry::new().register(
        "long",
        waits_for_cancel(&probe, || Err(HandlerError::cancelled())),
    );
    let worker = start(&source, registry, config(1));

    assert!(eventually(|| probe.started()));
    source.request_cancel(&id);
    assert!(source.wait_until(PATIENCE, |v| v.outcome(&id).is_some()));

    let req = source
        .outcome(&id)
        .and_then(|o| o.failure().cloned())
        .expect("failed");
    assert_eq!(req.error, "cancelled on request");
    assert_eq!(req.kind.as_deref(), Some(failure_kind::CANCELLED));
    assert!(!req.retryable);
    assert_eq!(probe.reason(), Some(CancelReason::Requested));
    assert_eq!(worker.stop().stats.cancelled, 1);
}

#[test]
fn timeouts_cancel_cooperative_handlers() {
    let source = Arc::new(MemorySource::new());
    source.set_requeue(false);
    let id = source.push(spec("slow"));
    let probe = Arc::new(Probe::default());
    let registry = Registry::new().register_with(
        "slow",
        waits_for_cancel(&probe, || Err(HandlerError::cancelled())),
        KindOptions::new().timeout(Duration::from_millis(30)),
    );
    let worker = start(&source, registry, config(1));

    assert!(source.wait_until(PATIENCE, |v| v.outcome(&id).is_some()));
    let req = source
        .outcome(&id)
        .and_then(|o| o.failure().cloned())
        .expect("failed");
    assert_eq!(req.error, "timed out after 30ms");
    assert_eq!(req.kind.as_deref(), Some(failure_kind::TIMEOUT));
    assert!(req.retryable);
    assert_eq!(probe.reason(), Some(CancelReason::TimedOut));
    let stats = worker.stop().stats;
    assert_eq!((stats.timed_out, stats.abandoned, stats.retried), (1, 0, 1));
}

#[test]
fn the_shorter_of_job_and_kind_timeout_applies() {
    let source = Arc::new(MemorySource::new());
    source.set_requeue(false);
    let id = source.push(spec("slow").timeout(Duration::from_millis(40)));
    let probe = Arc::new(Probe::default());
    let registry = Registry::new().register_with(
        "slow",
        waits_for_cancel(&probe, || Err(HandlerError::cancelled())),
        KindOptions::new().timeout(Duration::from_secs(60)),
    );
    let worker = start(&source, registry, config(1));

    assert!(source.wait_until(PATIENCE, |v| v.outcome(&id).is_some()));
    let req = source
        .outcome(&id)
        .and_then(|o| o.failure().cloned())
        .expect("failed");
    assert_eq!(req.error, "timed out after 40ms");
    drop(worker);
}

#[test]
fn handlers_that_ignore_timeouts_are_abandoned_after_the_grace_period() {
    let source = Arc::new(MemorySource::new());
    source.set_requeue(false);
    let id = source.push(spec("stubborn"));
    let gate = Gate::new();
    let g = Arc::clone(&gate);
    let registry = Registry::new().register_with(
        "stubborn",
        handler_fn(move |_: &mut JobContext| {
            g.pass();
            Ok(Outcome::with("too late"))
        }),
        KindOptions::new().timeout(Duration::from_millis(20)),
    );
    let worker = start(
        &source,
        registry,
        config(1).cancel_grace(Duration::from_millis(30)),
    );

    // The watchdog reports the timeout itself.
    assert!(source.wait_until(PATIENCE, |v| v.outcome(&id).is_some()));
    let req = source
        .outcome(&id)
        .and_then(|o| o.failure().cloned())
        .expect("failed");
    assert_eq!(
        (req.error.as_str(), req.retryable),
        ("timed out after 20ms", true)
    );
    // The job is forgotten right after the report goes out.
    assert!(eventually(|| worker.stats().running == 0));
    let stats = worker.stats();
    assert_eq!((stats.timed_out, stats.abandoned), (1, 1));
    assert_eq!(stats.busy_slots, 1, "the thread is still stuck");

    // Once the handler returns, its result goes nowhere and the slot frees.
    gate.open();
    assert!(eventually(|| worker.stats().busy_slots == 0));
    assert_eq!(source.view(|v| v.outcomes_for(&id).len()), 1);
    assert_eq!(worker.stop().stats.succeeded, 0);
}

#[test]
fn a_stuck_handler_keeps_its_slot() {
    let source = Arc::new(MemorySource::new());
    source.set_requeue(false);
    let stuck = source.push(spec("stubborn"));
    let gate = Gate::new();
    let g = Arc::clone(&gate);
    let registry = Registry::new()
        .register_with(
            "stubborn",
            handler_fn(move |_: &mut JobContext| {
                g.pass();
                Ok(Outcome::done())
            }),
            KindOptions::new().timeout(Duration::from_millis(10)),
        )
        .register("echo", EchoHandler);
    let worker = start(
        &source,
        registry,
        config(1).cancel_grace(Duration::from_millis(10)),
    );

    assert!(source.wait_until(PATIENCE, |v| v.outcome(&stuck).is_some()));
    let next = source.push(spec("echo"));
    // With the only thread stuck, the worker has no capacity to lease.
    assert_eq!(worker.stats().idle_slots(), 0);
    assert!(!source.wait_until(Duration::from_millis(20), |v| v.is_leased(&next)));
    assert!(source.outcome(&next).is_none());

    gate.open();
    assert!(source.wait_until(PATIENCE, |v| v.outcome(&next).is_some()));
    drop(worker);
}

#[test]
fn source_outages_back_off_and_log_once_per_streak() {
    let source = Arc::new(MemorySource::new());
    source.fail_next_leases(5);
    let id = source.push(spec("echo"));
    let capture = CaptureLogger::new();
    let worker = start(
        &source,
        Registry::new().register("echo", EchoHandler),
        config_logging_to(1, capture.clone()),
    );

    assert!(source.wait_until(PATIENCE, |v| v.outcome(&id).is_some()));
    let stats = worker.stop().stats;
    assert_eq!(stats.lease_errors, 5);
    assert_eq!(
        stats.last_error.as_deref(),
        Some("unavailable: injected lease failure")
    );
    let count = |needle: &str| {
        capture
            .messages()
            .iter()
            .filter(|m| m.contains(needle))
            .count()
    };
    assert_eq!(count("leasing jobs failed; backing off"), 1);
    assert_eq!(count("leasing jobs failed again"), 4);
    assert_eq!(count("job source reachable again"), 1);
    let recovered = capture
        .find(|r| r.message == "job source reachable again")
        .expect("recovery logged");
    assert_eq!(recovered.field("failed_attempts"), Some("5"));
}

#[test]
fn a_source_that_is_down_is_retried_until_it_returns() {
    let source = Arc::new(MemorySource::new());
    source.set_down(Some(ApiError::unavailable("maintenance")));
    let id = source.push(spec("echo"));
    let worker = start(
        &source,
        Registry::new().register("echo", EchoHandler),
        config(1),
    );

    assert!(source.wait_until(PATIENCE, |v| v.lease_calls() >= 3));
    assert!(source.outcome(&id).is_none());
    source.set_down(None);
    assert!(source.wait_until(PATIENCE, |v| v.outcome(&id).is_some()));
    assert!(worker.stop().stats.lease_errors >= 3);
}

#[test]
fn results_are_redelivered_after_transient_report_errors() {
    let source = Arc::new(MemorySource::new());
    source.fail_next_reports(2);
    let id = source.push(spec("echo"));
    let worker = start(
        &source,
        Registry::new().register("echo", EchoHandler),
        config(1),
    );

    assert!(source.wait_until(PATIENCE, |v| v.outcome(&id).is_some()));
    assert!(matches!(
        source.outcome(&id),
        Some(JobOutcome::Completed(_))
    ));
    let stats = worker.stop().stats;
    assert_eq!((stats.succeeded, stats.report_errors), (1, 0));
}

#[test]
fn undeliverable_results_are_counted_and_the_job_is_let_go() {
    let source = Arc::new(MemorySource::new());
    source.fail_next_reports(10);
    let id = source.push(spec("echo"));
    let worker = start(
        &source,
        Registry::new().register("echo", EchoHandler),
        config(1).report_attempts(2),
    );

    assert!(eventually(|| worker.stats().report_errors == 1));
    assert!(source.outcome(&id).is_none());
    let stats = worker.stop().stats;
    assert_eq!((stats.succeeded, stats.running), (0, 0));
    assert_eq!(
        stats.last_error.as_deref(),
        Some("unavailable: injected report failure")
    );
}

#[test]
fn progress_survives_failed_heartbeats() {
    let source = Arc::new(MemorySource::new());
    source.fail_next_heartbeats(3);
    let id = source.push(spec("report"));
    let gate = Gate::new();
    let g = Arc::clone(&gate);
    let registry = Registry::new().register(
        "report",
        handler_fn(move |ctx: &mut JobContext| {
            ctx.progress(10.0, "started");
            g.pass();
            Ok(Outcome::done())
        }),
    );
    let worker = start(&source, registry, config(1));

    assert!(source.wait_until(PATIENCE, |v| !v.progress_for(&id).is_empty()));
    gate.open();
    assert!(source.wait_until(PATIENCE, |v| v.completed() == 1));
    let stats = worker.stop().stats;
    assert_eq!(stats.heartbeat_errors, 3);
    assert_eq!(source.view(|v| v.progress_for(&id).len()), 1);
}
