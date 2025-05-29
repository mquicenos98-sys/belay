//! Things going wrong around a job: lost leases, cancellation requests,
//! timeouts (cooperative and not), source outages, and failed heartbeats
//! and reports.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use belay_proto::api::ApiError;
use belay_worker::builtin::EchoHandler;
use belay_worker::testing::{JobOutcome, MemorySource};
use belay_worker::{
    failure_kind, handler_fn, CancelReason, HandlerError, JobContext, KindOptions, Outcome,
    Registry,
};

use support::{config, eventually, spec, start, Gate, PATIENCE};

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
