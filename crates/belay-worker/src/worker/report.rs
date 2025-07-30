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

/// Makes the `complete` or `fail` call, retrying transient errors.
fn deliver(shared: &Shared, id: JobId, token: LeaseToken, report: &Report) -> Result<(), ApiError> {
    let attempts = shared.config.report_attempts.max(1);
    let mut rng = thread_rng();
    let mut attempt = 1;
    loop {
        let result = match report {
            Report::Complete(value) => shared.source.complete(
                &id,
                &CompleteRequest {
                    token,
                    result: value.clone(),
                },
            ),
            Report::Fail(e) => shared.source.fail(&id, &fail_request(token, e)),
        };
        match result {
            Ok(()) => return Ok(()),
            Err(e) if attempt >= attempts || !worth_retrying(&e) => return Err(e),
            Err(e) => {
                debug!("reporting a job result failed; retrying"; job = %id, attempt, error = %e);
                thread::sleep(shared.config.source_backoff.delay_for(attempt, &mut rng));
                attempt += 1;
            }
        }
    }
}

/// Errors a repeat of the same call might not hit.
fn worth_retrying(e: &ApiError) -> bool {
    matches!(
        e.code,
        ErrorCode::Unavailable
            | ErrorCode::ResourceExhausted
            | ErrorCode::Internal
            | ErrorCode::Conflict
    )
}

/// The wire form of a failure, cleaned so the source cannot reject it.
pub(crate) fn fail_request(token: LeaseToken, e: &HandlerError) -> FailRequest {
    let error = if e.message.trim().is_empty() {
        "handler failed without a message".to_owned()
    } else {
        e.message.clone()
    };
    FailRequest {
        token,
        error,
        kind: e.kind.as_deref().and_then(sanitize_kind),
        retryable: e.retryable,
        retry_in: e.retry_in.map(|d| d.min(MAX_DELAY)),
    }
}

/// Maps a free-form kind onto the label rule (letters, digits and
/// `_ . : / -`, starting with a letter or digit, at most 64 characters)
/// instead of letting the source reject the whole failure report.
pub(crate) fn sanitize_kind(kind: &str) -> Option<String> {
    let cleaned: String = kind
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '/' | '-') {
                c
            } else {
                '_'
            }
        })
        .skip_while(|c| !c.is_ascii_alphanumeric())
        .take(MAX_ERROR_KIND_LEN)
        .collect();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// The text of a panic payload (`panic!` with a literal or a format).
pub(crate) fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_are_cleaned_to_the_label_rule() {
        assert_eq!(sanitize_kind("http_503").as_deref(), Some("http_503"));
        assert_eq!(sanitize_kind("not ok").as_deref(), Some("not_ok"));
        assert_eq!(sanitize_kind("-x/y").as_deref(), Some("x/y"));
        assert_eq!(sanitize_kind("é").as_deref(), None);
        assert_eq!(sanitize_kind("").as_deref(), None);
        assert_eq!(sanitize_kind(&"k".repeat(100)).map(|k| k.len()), Some(64));
    }

    #[test]
    fn fail_requests_are_always_acceptable() {
        let token = LeaseToken::new(9);
        let e = HandlerError::retry("  ")
            .with_kind("rate limited")
            .retry_after(Duration::from_secs(400 * 86_400));
        let req = fail_request(token, &e);
        assert_eq!(req.error, "handler failed without a message");
        assert_eq!(req.kind.as_deref(), Some("rate_limited"));
        assert_eq!(req.retry_in, Some(MAX_DELAY));
        assert!(req.retryable);
        assert!(belay_proto::Validate::validate(&req).is_ok());
    }

    #[test]
    fn canned_failures_say_what_happened() {
        let t = timeout_error(Some(Duration::from_millis(1500)));
        assert_eq!(t.message, "timed out after 1s500ms");
        assert_eq!((t.retryable, t.kind.as_deref()), (true, Some("timeout")));
        assert_eq!(timeout_error(None).message, "timed out");
        let s = shutdown_error();
        assert_eq!(s.message, "worker shutting down");
        assert_eq!(s.retry_in, Some(Duration::ZERO));
        assert!(s.retryable);
    }

    #[test]
    fn panic_payloads_become_text() {
        let literal: Box<dyn Any + Send> = Box::new("static text");
        assert_eq!(panic_message(&*literal), "static text");
        let formatted: Box<dyn Any + Send> = Box::new(format!("index {}", 3));
        assert_eq!(panic_message(&*formatted), "index 3");
        let other: Box<dyn Any + Send> = Box::new(42u8);
        assert_eq!(panic_message(&*other), "non-string panic payload");
    }

    #[test]
    fn only_transient_errors_are_retried() {
        assert!(worth_retrying(&ApiError::unavailable("down")));
        assert!(worth_retrying(&ApiError::internal("oops")));
        assert!(!worth_retrying(&ApiError::invalid_argument("bad")));
        assert!(!worth_retrying(&ApiError::lease_lost(JobId::generate())));
    }
}
