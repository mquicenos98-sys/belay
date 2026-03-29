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
        let progress = job.signals.take_progress();
        let req = HeartbeatRequest {
            token: job.token,
            extend_by: None,
            progress: progress.clone(),
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
                if let Some(p) = progress {
                    job.signals.restore_progress(p);
                }
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

/// Body of the watchdog thread.
pub(crate) fn run_watchdog(shared: &Shared) {
    let grace = shared.config.cancel_grace;
    let mut state = lock(&shared.state);
    loop {
        if state.stopping {
            return;
        }
        let now = Instant::now();
        let mut next_wake: Option<Instant> = None;
        let mut expired = Vec::new();
        for job in state.jobs.values() {
            if job.phase() != Phase::Running {
                continue;
            }
            let Some(deadline) = job.timing().and_then(|t| t.deadline) else {
                continue;
            };
            let wake_at = if now < deadline {
                deadline
            } else {
                if !job
                    .deadline_hit
                    .swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    Counters::bump(&shared.counters.timed_out);
                    job.signals.cancel(CancelReason::TimedOut);
                    warn!(
                        "job ran past its timeout; asking the handler to stop";
                        job = %job.id(),
                        kind = job.kind(),
                        grace,
                    );
                }
                match deadline.checked_add(grace) {
                    Some(give_up) if now < give_up => give_up,
                    Some(_) => {
                        expired.push(Arc::clone(job));
                        continue;
                    }
                    None => continue,
                }
            };
            next_wake = Some(next_wake.map_or(wake_at, |w| w.min(wake_at)));
        }
        if !expired.is_empty() {
            drop(state);
            for job in expired {
                give_up_on(shared, &job);
            }
            state = lock(&shared.state);
            continue;
        }
        state = match next_wake {
            Some(at) => wait_timeout(&shared.changed, state, at.saturating_duration_since(now)).0,
            None => wait(&shared.changed, state),
        };
    }
}

/// Reports a timeout for a handler that ignored cancellation.
fn give_up_on(shared: &Shared, job: &Tracked) {
    if !job.abandon() {
        // It returned just now; its executor reports.
        return;
    }
    Counters::bump(&shared.counters.abandoned);
    warn!(
        "handler ignored cancellation past the grace period; reporting a timeout and abandoning it";
        job = %job.id(),
        kind = job.kind(),
    );
    let timeout = job.timing().and_then(|t| t.timeout);
    settle(
        shared,
        job.id(),
        job.token,
        job.kind(),
        &Report::Fail(timeout_error(timeout)),
    );
    shared.forget(job.id());
}
