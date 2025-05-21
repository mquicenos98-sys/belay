//! [`EchoHandler`], [`SleepHandler`] and [`FailHandler`].

use std::time::Duration;

use belay_common::format_duration;
use belay_json::{json, FromJson, FromJsonError, ObjectReader, Value};
use belay_proto::wire::ReaderExt;

use crate::context::JobContext;
use crate::handler::{Handler, HandlerError, Outcome};

/// Returns its payload as the result.
#[derive(Clone, Copy, Debug, Default)]
pub struct EchoHandler;

impl Handler for EchoHandler {
    fn handle(&self, ctx: &mut JobContext) -> Result<Outcome, HandlerError> {
        Ok(Outcome::with(ctx.payload().clone()))
    }
}

/// Sleeps cooperatively, reporting progress, then succeeds with
/// `{"slept": "<duration>"}`.
///
/// Payload: `{"for": "1.5s"}` (any duration string; absent or `null`
/// means the handler's default). Stops early with
/// [`HandlerError::cancelled`] when the job is cancelled.
#[derive(Clone, Copy, Debug)]
pub struct SleepHandler {
    default: Duration,
    max: Duration,
}

#[derive(Debug)]
struct SleepPayload {
    duration: Option<Duration>,
}

impl FromJson for SleepPayload {
    fn from_json(value: &Value) -> Result<SleepPayload, FromJsonError> {
        if value.is_null() {
            return Ok(SleepPayload { duration: None });
        }
        let r = ObjectReader::new(value, "SleepPayload")?;
        let payload = SleepPayload {
            duration: r.opt_duration("for")?,
        };
        r.deny_unknown_fields()?;
        Ok(payload)
    }
}

impl SleepHandler {
    /// Sleeps one second by default and at most an hour.
    #[must_use]
    pub fn new() -> SleepHandler {
        SleepHandler {
            default: Duration::from_secs(1),
            max: Duration::from_secs(3600),
        }
    }

    /// Sets the duration used when the payload has none.
    #[must_use]
    pub fn default_duration(mut self, d: Duration) -> SleepHandler {
        self.default = d;
        self
    }

    /// Rejects payloads asking for longer than `max`.
    #[must_use]
    pub fn max(mut self, max: Duration) -> SleepHandler {
        self.max = max;
        self
    }
}

impl Default for SleepHandler {
    fn default() -> SleepHandler {
        SleepHandler::new()
    }
}

impl Handler for SleepHandler {
    fn handle(&self, ctx: &mut JobContext) -> Result<Outcome, HandlerError> {
        const STEPS: u32 = 10;
        let total = ctx
            .payload_as::<SleepPayload>()?
            .duration
            .unwrap_or(self.default);
        if total > self.max {
            return Err(HandlerError::invalid_payload(format!(
                "sleep of {} exceeds the {} limit",
                format_duration(total),
                format_duration(self.max)
            )));
        }
        let step = total / STEPS;
        for i in 1..=STEPS {
            // The last step absorbs the rounding of `total / STEPS`.
            let d = if i == STEPS {
                total - step * (STEPS - 1)
            } else {
                step
            };
            if !ctx.sleep_or_cancel(d) {
                return Err(HandlerError::cancelled());
            }
            let slept = if i == STEPS { total } else { step * i };
            ctx.progress(
                (i * 100 / STEPS) as f32,
                format!("slept {}", format_duration(slept)),
            );
        }
        Ok(Outcome::with(json!({"slept": (format_duration(total))})))
    }
}

/// Fails on purpose, for testing retry and failure paths.
///
/// Configured with builder methods; a payload object may override any
/// setting: `{"message": "...", "retryable": false, "kind": "...",
/// "retry_in": "10s", "panic": true, "succeed_on_attempt": 3}`. With
/// `succeed_on_attempt`, attempts from that one on succeed with
/// `{"attempt": n}`, which is handy for exercising retries end to end.
#[derive(Clone, Debug)]
pub struct FailHandler {
    message: String,
    retryable: bool,
    kind: Option<String>,
    retry_in: Option<Duration>,
    panic: bool,
    succeed_on_attempt: Option<u32>,
}

impl FailHandler {
    /// Fails every attempt with a retryable `message`.
    #[must_use]
    pub fn new(message: impl Into<String>) -> FailHandler {
        FailHandler {
            message: message.into(),
            retryable: true,
            kind: None,
            retry_in: None,
            panic: false,
            succeed_on_attempt: None,
        }
    }

    /// Makes the failure non-retryable.
    #[must_use]
    pub fn fatal(mut self) -> FailHandler {
        self.retryable = false;
        self
    }

    /// Sets the failure kind.
    #[must_use]
    pub fn kind(mut self, kind: impl Into<String>) -> FailHandler {
        self.kind = Some(kind.into());
        self
    }

    /// Asks for a retry after exactly `delay`.
    #[must_use]
    pub fn retry_after(mut self, delay: Duration) -> FailHandler {
        self.retry_in = Some(delay);
        self
    }

    /// Panics instead of returning an error.
    #[must_use]
    pub fn panicking(mut self) -> FailHandler {
        self.panic = true;
        self
    }

    /// Succeeds from attempt `n` on.
    #[must_use]
    pub fn succeed_on_attempt(mut self, n: u32) -> FailHandler {
        self.succeed_on_attempt = Some(n);
        self
    }

    /// This configuration with the payload's overrides applied.
    fn with_overrides(&self, payload: &Value) -> Result<FailHandler, FromJsonError> {
        if payload.is_null() {
            return Ok(self.clone());
        }
        let r = ObjectReader::new(payload, "FailPayload")?;
        let merged = FailHandler {
            message: r.or_else("message", || self.message.clone())?,
            retryable: r.or("retryable", self.retryable)?,
            kind: r.opt("kind")?.or_else(|| self.kind.clone()),
            retry_in: r.opt_duration("retry_in")?.or(self.retry_in),
            panic: r.or("panic", self.panic)?,
            succeed_on_attempt: r.opt("succeed_on_attempt")?.or(self.succeed_on_attempt),
        };
        r.deny_unknown_fields()?;
        Ok(merged)
    }
}

impl Handler for FailHandler {
    fn handle(&self, ctx: &mut JobContext) -> Result<Outcome, HandlerError> {
        let plan = self.with_overrides(ctx.payload())?;
        if let Some(n) = plan.succeed_on_attempt {
            if ctx.attempt() >= n {
                return Ok(Outcome::with(json!({"attempt": (ctx.attempt())})));
            }
        }
        if plan.panic {
            panic!("{}", plan.message);
        }
        let mut error = if plan.retryable {
            HandlerError::retry(plan.message)
        } else {
            HandlerError::fatal(plan.message)
        };
        error.kind = plan.kind;
        if let Some(delay) = plan.retry_in {
            error = error.retry_after(delay);
        }
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::CancelReason;
    use belay_common::Timestamp;
    use belay_proto::{Job, JobId, JobSpec, Progress};
    use std::panic::{self, AssertUnwindSafe};
    use std::thread;

    fn ctx(payload: Value, attempt: u32) -> JobContext {
        let mut job = Job::new(
            JobId::generate(),
            JobSpec::new("default", "test").payload(payload),
            Timestamp::from_secs(1),
        );
        job.attempt = attempt;
        JobContext::detached(job)
    }

    #[test]
    fn echo_returns_the_payload() {
        let payload = json!({"a": [1, 2], "b": null});
        let outcome = EchoHandler.handle(&mut ctx(payload.clone(), 1)).unwrap();
        assert_eq!(outcome.into_result(), Some(payload));
    }

    #[test]
    fn sleep_reports_progress_and_the_duration() {
        let mut c = ctx(json!({"for": "20ms"}), 1);
        let outcome = SleepHandler::new().handle(&mut c).unwrap();
        assert_eq!(outcome.into_result(), Some(json!({"slept": "20ms"})));
        assert_eq!(
            c.pending_progress(),
            Some(Progress::percent(100.0).with_message("slept 20ms"))
        );
    }

    #[test]
    fn sleep_uses_its_default_and_enforces_its_maximum() {
        let handler = SleepHandler::new()
            .default_duration(Duration::from_millis(1))
            .max(Duration::from_secs(1));
        assert!(handler.handle(&mut ctx(Value::Null, 1)).is_ok());
        let err = handler
            .handle(&mut ctx(json!({"for": "2h"}), 1))
            .unwrap_err();
        assert_eq!(
            err.message,
            "invalid payload: sleep of 2h exceeds the 1s limit"
        );
        let err = handler.handle(&mut ctx(json!({"for": 5}), 1)).unwrap_err();
        assert!(!err.retryable);
        let err = handler
            .handle(&mut ctx(json!({"fr": "1s"}), 1))
            .unwrap_err();
        assert!(
            err.message.contains(r#"did you mean "for"?"#),
            "{}",
            err.message
        );
    }

    #[test]
    fn sleep_stops_when_cancelled() {
        let mut c = ctx(json!({"for": "1h"}), 1);
        let handle = c.cancel_handle();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            handle.cancel();
        });
        let err = SleepHandler::new().handle(&mut c).unwrap_err();
        assert!(err.is_cancellation());
        assert_eq!(c.cancel_reason(), Some(CancelReason::Requested));
        canceller.join().expect("canceller thread");
    }

    #[test]
    fn fail_uses_its_configuration() {
        let handler = FailHandler::new("nope")
            .fatal()
            .kind("test")
            .retry_after(Duration::from_secs(5));
        let err = handler.handle(&mut ctx(Value::Null, 1)).unwrap_err();
        assert_eq!(err.message, "nope");
        assert_eq!(err.kind.as_deref(), Some("test"));
        assert!(err.retryable, "retry_after implies retryable");
        assert_eq!(err.retry_in, Some(Duration::from_secs(5)));

        let err = FailHandler::new("x")
            .fatal()
            .handle(&mut ctx(Value::Null, 1))
            .unwrap_err();
        assert!(!err.retryable);
    }

    #[test]
    fn fail_payload_overrides_and_eventual_success() {
        let handler = FailHandler::new("default message");
        let payload = json!({"message": "custom", "retryable": false, "succeed_on_attempt": 3});
        let err = handler.handle(&mut ctx(payload.clone(), 2)).unwrap_err();
        assert_eq!((err.message.as_str(), err.retryable), ("custom", false));
        let ok = handler.handle(&mut ctx(payload, 3)).unwrap();
        assert_eq!(ok.into_result(), Some(json!({"attempt": 3})));

        let err = handler
            .handle(&mut ctx(json!({"retry": true}), 1))
            .unwrap_err();
        assert_eq!(err.kind.as_deref(), Some("invalid_payload"));
    }

    #[test]
    fn fail_can_panic() {
        let handler = FailHandler::new("kaboom").panicking();
        let caught = panic::catch_unwind(AssertUnwindSafe(|| {
            handler.handle(&mut ctx(Value::Null, 1))
        }));
        let payload = caught.expect_err("handler panics");
        assert_eq!(
            payload.downcast_ref::<String>().map(String::as_str),
            Some("kaboom")
        );
    }
}
