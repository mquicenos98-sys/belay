//! The two threads that watch running jobs: heartbeats (lease renewal,
//! progress delivery, cancellation requests, lost leases) and the timeout
//! watchdog.
//!
//! Heartbeats go out in one pass over every job held, every
//! `heartbeat_every`: the protocol renews one lease per call, so a "batch"
//! is a sequence of calls, but one timer and one log line per pass keep
//! the cost flat as concurrency grows. Waiting jobs are included, since
//! their leases must survive until they start.
//!
//! The watchdog sleeps until the nearest deadline. At the deadline it sets
//! the job's cancellation flag; if the handler has not returned
//! `cancel_grace` later, it reports a retryable timeout and abandons the
//! job. The thread itself cannot be stopped, so it keeps its executor
//! slot until the handler returns, and whatever it returns is dropped.

use std::sync::Arc;
use std::time::Instant;

use belay_common::sync::{lock, wait, wait_timeout, Shutdown, Ticker};
use belay_common::{info, warn};
use belay_proto::api::{ErrorCode, HeartbeatRequest};

use crate::context::CancelReason;
use crate::stats::Counters;
use crate::worker::report::{settle, timeout_error, Report};
use crate::worker::state::{Phase, Shared, Tracked};

/// Body of the heartbeat thread: a pass every period until `stop`.
pub(crate) fn run_heartbeats(shared: &Shared, stop: &Shutdown) {
    for _tick in Ticker::new(shared.config.effective_heartbeat(), stop.clone()) {
        heartbeat_pass(shared);
    }
}

/// Renews every lease held, once.
pub(crate) fn heartbeat_pass(shared: &Shared) {
    let jobs: Vec<Arc<Tracked>> = lock(&shared.state).jobs.values().cloned().collect();
    let mut failed = 0u32;
    let mut first_error = None;
    for job in jobs {
        if job.phase() == Phase::Abandoned {
            continue;
        }
        let req = HeartbeatRequest {
            token: job.token,
            extend_by: None,
            progress: job.signals.take_progress(),
        };
        match shared.source.heartbeat(&job.id(), &req) {
            Ok(resp) => {
                if resp.cancel_requested && job.signals.cancel(CancelReason::Requested) {
                    Counters::bump(&shared.counters.cancelled);
                    info!(
                        "cancellation requested; asking the handler to stop";
                        job = %job.id(),
                        kind = job.kind(),
                    );
                }
            }
            Err(e) if e.is(ErrorCode::LeaseLost) => lease_lost(shared, &job),
            Err(e) => {
                Counters::bump(&shared.counters.heartbeat_errors);
                failed += 1;
                first_error.get_or_insert(e);
            }
        }
    }
    if let Some(e) = first_error {
        shared.counters.note_error(&e);
        warn!("heartbeats failed; leases may expire"; failed, error = %e);
    }
}

/// The source says someone else holds the job now: stop the handler and
/// make sure nothing about the job is reported any more.
fn lease_lost(shared: &Shared, job: &Tracked) {
    // A job already finishing learns about the loss from its own report.
    if job.abandon() {
        job.signals.cancel(CancelReason::LeaseLost);
        Counters::bump(&shared.counters.abandoned);
        warn!(
            "lease lost; abandoning the job, its result will be dropped";
            job = %job.id(),
            kind = job.kind(),
        );
        shared.forget(job.id());
    }
}
