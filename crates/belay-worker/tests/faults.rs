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
