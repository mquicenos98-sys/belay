//! What a job runs: the [`Handler`] trait, what a handler returns
//! ([`Outcome`] or [`HandlerError`]), and [`handler_fn`] for closures.

use std::fmt;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use belay_json::{FromJsonError, ToJson, Value};

use crate::context::JobContext;

/// Machine-readable failure classes the worker itself reports, sent as the
/// `kind` of a failed attempt. Handlers are free to use their own kinds
/// (letters, digits and `_ . : / -`, at most 64 characters).
pub mod failure_kind {
    /// The handler noticed cancellation and stopped
    /// ([`HandlerError::cancelled`](crate::HandlerError::cancelled)).
    pub const CANCELLED: &str = "cancelled";
    /// The attempt ran past its timeout.
    pub const TIMEOUT: &str = "timeout";
    /// The handler panicked.
    pub const PANIC: &str = "panic";
    /// The worker shut down before the attempt finished.
    pub const SHUTDOWN: &str = "shutdown";
    /// No handler is registered for the job's kind.
    pub const UNKNOWN_KIND: &str = "unknown_kind";
    /// The payload does not decode as the handler expects.
    pub const INVALID_PAYLOAD: &str = "invalid_payload";
    /// The handler's result exceeds
    /// [`MAX_RESULT_BYTES`](belay_proto::limits::MAX_RESULT_BYTES).
    pub const RESULT_TOO_LARGE: &str = "result_too_large";
    /// An I/O error converted with `?`.
    pub const IO: &str = "io";
}

/// Runs jobs of one kind.
///
/// Handlers are shared by all executor threads, hence `Send + Sync`; keep
/// per-job state in locals or in the [`JobContext`]. A handler that runs
/// for long should look at [`JobContext::is_cancelled`] now and then (or
/// sleep with [`JobContext::sleep_or_cancel`]) so timeouts, cancellation
/// requests and shutdown can stop it: threads cannot be killed, so a
/// handler that never returns keeps its executor slot forever.
///
/// Returning `Err` fails the attempt; whether the job is tried again is
/// up to [`HandlerError::retryable`] and the job's remaining attempts. A
/// panic is caught and reported as a retryable failure of kind
/// [`failure_kind::PANIC`].
pub trait Handler: Send + Sync {
    /// Runs one attempt of the job described by `ctx`.
    fn handle(&self, ctx: &mut JobContext) -> Result<Outcome, HandlerError>;
}

impl<H: Handler + ?Sized> Handler for Arc<H> {
    fn handle(&self, ctx: &mut JobContext) -> Result<Outcome, HandlerError> {
        (**self).handle(ctx)
    }
}

impl<H: Handler + ?Sized> Handler for Box<H> {
    fn handle(&self, ctx: &mut JobContext) -> Result<Outcome, HandlerError> {
        (**self).handle(ctx)
    }
}

/// A [`Handler`] made from a closure by [`handler_fn`].
#[derive(Clone)]
pub struct FnHandler<F>(F);

impl<F> fmt::Debug for FnHandler<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FnHandler(..)")
    }
}

impl<F> Handler for FnHandler<F>
where
    F: Fn(&mut JobContext) -> Result<Outcome, HandlerError> + Send + Sync,
{
    fn handle(&self, ctx: &mut JobContext) -> Result<Outcome, HandlerError> {
        (self.0)(ctx)
    }
}

/// Turns a closure into a [`Handler`].
///
/// ```
/// use belay_worker::{handler_fn, HandlerError, JobContext, Outcome};
///
/// let greet = handler_fn(|ctx: &mut JobContext| {
///     let name: String = ctx.payload_as()?;
///     if name.is_empty() {
///         return Err(HandlerError::fatal("name must not be empty"));
///     }
///     Ok(Outcome::with(format!("hello, {name}")))
/// });
/// # let _ = greet;
/// ```
pub fn handler_fn<F>(f: F) -> FnHandler<F>
where
    F: Fn(&mut JobContext) -> Result<Outcome, HandlerError> + Send + Sync,
{
    FnHandler(f)
}

/// A successful attempt.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// The job is done; the value, if any, is stored as the job's result.
    /// `Some(Value::Null)` stores an explicit `null`.
    Done(Option<Value>),
}

impl Outcome {
    /// Done, without a result.
    #[must_use]
    pub fn done() -> Outcome {
        Outcome::Done(None)
    }

    /// Done, with `result`.
    #[must_use]
    pub fn with(result: impl Into<Value>) -> Outcome {
        Outcome::Done(Some(result.into()))
    }

    /// Done, with the JSON form of `result`.
    #[must_use]
    pub fn json<T: ToJson + ?Sized>(result: &T) -> Outcome {
        Outcome::Done(Some(result.to_json()))
    }

    /// The result, if any.
    #[must_use]
    pub fn result(&self) -> Option<&Value> {
        match self {
            Outcome::Done(result) => result.as_ref(),
        }
    }

    /// The result, owned.
    #[must_use]
    pub fn into_result(self) -> Option<Value> {
        match self {
            Outcome::Done(result) => result,
        }
    }
}

impl From<Value> for Outcome {
    fn from(result: Value) -> Outcome {
        Outcome::Done(Some(result))
    }
}

impl From<()> for Outcome {
    fn from((): ()) -> Outcome {
        Outcome::done()
    }
}

/// A failed attempt, as reported to the source in a
/// [`FailRequest`](belay_proto::api::FailRequest).
///
/// ```
/// use std::time::Duration;
/// use belay_worker::HandlerError;
///
/// let throttled = HandlerError::retry("upstream answered 429")
///     .with_kind("http_429")
///     .retry_after(Duration::from_secs(30));
/// assert!(throttled.retryable);
///
/// let bad_input = HandlerError::fatal("unknown template \"welcom\"");
/// assert!(!bad_input.retryable);
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandlerError {
    /// What went wrong, for people.
    pub message: String,
    /// Machine-readable class, e.g. `timeout` or `http_503`. Characters
    /// outside the allowed set are replaced when the failure is reported.
    pub kind: Option<String>,
    /// Whether another attempt may succeed.
    pub retryable: bool,
    /// Retry after exactly this long instead of the job's backoff policy.
    pub retry_in: Option<Duration>,
}

impl HandlerError {
    /// A failure worth retrying (subject to the job's remaining attempts).
    #[must_use]
    pub fn retry(message: impl Into<String>) -> HandlerError {
        HandlerError {
            message: message.into(),
            kind: None,
            retryable: true,
            retry_in: None,
        }
    }

    /// A failure that no retry will fix: the job goes straight to `dead`.
    #[must_use]
    pub fn fatal(message: impl Into<String>) -> HandlerError {
        HandlerError {
            retryable: false,
            ..HandlerError::retry(message)
        }
    }

    /// What a cooperative handler returns after noticing
    /// [`JobContext::is_cancelled`]. The worker replaces it with a failure
    /// that says why the job was stopped (timed out, cancelled on request,
    /// worker shutting down).
    #[must_use]
    pub fn cancelled() -> HandlerError {
        HandlerError::retry("cancelled").with_kind(failure_kind::CANCELLED)
    }

    /// The payload did not decode; never retryable.
    #[must_use]
    pub fn invalid_payload(reason: impl fmt::Display) -> HandlerError {
        HandlerError::fatal(format!("invalid payload: {reason}"))
            .with_kind(failure_kind::INVALID_PAYLOAD)
    }

    /// Sets the machine-readable kind.
    #[must_use]
    pub fn with_kind(mut self, kind: impl Into<String>) -> HandlerError {
        self.kind = Some(kind.into());
        self
    }

    /// Asks for the next attempt after exactly `delay`. Implies retryable.
    #[must_use]
    pub fn retry_after(mut self, delay: Duration) -> HandlerError {
        self.retryable = true;
        self.retry_in = Some(delay);
        self
    }

    /// Whether this is the error from [`cancelled`](Self::cancelled).
    #[must_use]
    pub fn is_cancellation(&self) -> bool {
        self.kind.as_deref() == Some(failure_kind::CANCELLED)
    }
}

impl fmt::Display for HandlerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HandlerError {}

/// Plain messages are retryable, matching the protocol's default for
/// failures that do not say otherwise.
impl From<String> for HandlerError {
    fn from(message: String) -> HandlerError {
        HandlerError::retry(message)
    }
}

impl From<&str> for HandlerError {
    fn from(message: &str) -> HandlerError {
        HandlerError::retry(message)
    }
}
