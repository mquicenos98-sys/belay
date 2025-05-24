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
