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
