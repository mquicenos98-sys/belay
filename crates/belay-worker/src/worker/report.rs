//! Turning what a handler did into a `complete` or `fail` call, and making
//! that call.
//!
//! Delivery is retried a few times on transient source errors: a result
//! that never arrives means the lease expires and the job runs again,
//! which is safe (delivery is at-least-once) but wasteful. Heartbeats keep
//! the lease alive while the retries happen, because the job stays in the
//! worker's table until delivery is over.

use std::any::Any;
use std::thread;
use std::time::Duration;

use belay_common::format_duration;
use belay_common::rand::thread_rng;
use belay_common::{debug, error, warn};
use belay_json::Value;
use belay_proto::api::{ApiError, CompleteRequest, ErrorCode, FailRequest};
use belay_proto::limits::{MAX_DELAY, MAX_ERROR_KIND_LEN, MAX_RESULT_BYTES};
use belay_proto::wire::encoded_len;
use belay_proto::{JobId, LeaseToken};

use crate::context::CancelReason;
use crate::handler::{failure_kind, HandlerError, Outcome};
use crate::stats::Counters;
use crate::worker::state::{Shared, Timing, Tracked};

/// What the executor got back from the handler chain.
pub(crate) type HandlerResult = thread::Result<Result<Outcome, HandlerError>>;

/// What to tell the source about an attempt.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Report {
    Complete(Option<Value>),
    Fail(HandlerError),
}

/// Decides the report for a handler that returned (or panicked).
pub(crate) fn report_for(
    shared: &Shared,
    job: &Tracked,
    result: HandlerResult,
    timing: Timing,
) -> Report {
    match result {
        Err(payload) => {
            Counters::bump(&shared.counters.panicked);
            let message = panic_message(&*payload);
            shared
                .counters
                .note_error(format_args!("handler panicked: {message}"));
            error!(
                "job handler panicked";
                job = %job.id(),
                kind = job.kind(),
                attempt = job.job.attempt,
                panic = message.as_str(),
            );
            Report::Fail(
                HandlerError::retry(format!("handler panicked: {message}"))
                    .with_kind(failure_kind::PANIC),
            )
        }
        Ok(Ok(outcome)) => {
            let result = outcome.into_result();
            if let Some(value) = &result {
                let size = encoded_len(value);
                if size > MAX_RESULT_BYTES {
                    return Report::Fail(
                        HandlerError::fatal(format!(
                            "result is {size} bytes as JSON; the limit is {MAX_RESULT_BYTES}"
                        ))
                        .with_kind(failure_kind::RESULT_TOO_LARGE),
                    );
                }
            }
            Report::Complete(result)
        }
        Ok(Err(e)) if e.is_cancellation() => Report::Fail(match job.signals.reason() {
            Some(CancelReason::TimedOut) => timeout_error(timing.timeout),
            Some(CancelReason::ShuttingDown) => shutdown_error(),
            Some(CancelReason::Requested) => {
                HandlerError::fatal("cancelled on request").with_kind(failure_kind::CANCELLED)
            }
            Some(CancelReason::LeaseLost) | None => e,
        }),
        Ok(Err(e)) => {
            shared.counters.note_error(&e);
            Report::Fail(e)
        }
    }
}

/// The failure reported for an attempt that ran out of time.
pub(crate) fn timeout_error(timeout: Option<Duration>) -> HandlerError {
    let message = match timeout {
        Some(t) => format!("timed out after {}", format_duration(t)),
        None => "timed out".to_owned(),
    };
    HandlerError::retry(message).with_kind(failure_kind::TIMEOUT)
}

/// The failure reported for jobs handed back at shutdown: retryable, and
/// due again at once so another worker picks them up.
pub(crate) fn shutdown_error() -> HandlerError {
    HandlerError::retry("worker shutting down")
        .with_kind(failure_kind::SHUTDOWN)
        .retry_after(Duration::ZERO)
}

/// Delivers `report` and updates the counters. The caller removes the job
/// from the table afterwards.
pub(crate) fn settle(shared: &Shared, id: JobId, token: LeaseToken, kind: &str, report: &Report) {
    let counters = &shared.counters;
    match deliver(shared, id, token, report) {
        Ok(()) => match report {
            Report::Complete(_) => {
                Counters::bump(&counters.succeeded);
                debug!("job succeeded"; job = %id, kind);
            }
            Report::Fail(e) => {
                Counters::bump(&counters.failed);
                if e.retryable {
                    Counters::bump(&counters.retried);
                }
                debug!(
                    "job failure reported";
                    job = %id,
                    kind,
                    error = %e,
                    retryable = e.retryable,
                );
            }
        },
        Err(e) if e.is(ErrorCode::LeaseLost) => {
            Counters::bump(&counters.abandoned);
            warn!("lease lost before the result was reported; result dropped"; job = %id, kind);
        }
        Err(e) => {
            Counters::bump(&counters.report_errors);
            counters.note_error(&e);
            error!(
                "could not report job result; the lease will expire and the job will run again";
                job = %id,
                kind,
                error = %e,
            );
        }
    }
}
