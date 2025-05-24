//! Code that runs around every handler: the [`Middleware`] trait, the
//! [`Next`] continuation, and two built-ins, [`LoggingMiddleware`] and
//! [`TimingMiddleware`].
//!
//! Panics, timeouts and cancellation are handled by the worker itself, not
//! by middleware, so they apply even to a registry without any: a
//! panicking handler unwinds through the middleware and is caught by the
//! executor (see [`Handler`] for how it is reported).

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use belay_common::log::Level;
use belay_common::sync::lock;

use crate::context::JobContext;
use crate::handler::{Handler, HandlerError, Outcome};

/// Wraps handler execution.
///
/// Call `next.run(ctx)` to continue down the chain (eventually reaching
/// the handler), or return without calling it to short-circuit.
///
/// ```
/// use belay_worker::middleware::Next;
/// use belay_worker::{HandlerError, JobContext, Middleware, Outcome};
///
/// /// Refuses jobs from tenants on a deny list.
/// struct DenyTenants(Vec<String>);
///
/// impl Middleware for DenyTenants {
///     fn call(&self, ctx: &mut JobContext, next: Next<'_>) -> Result<Outcome, HandlerError> {
///         let denied = ctx.job().spec.tenant.as_ref().is_some_and(|t| self.0.iter().any(|d| d == t.as_str()));
///         if denied {
///             return Err(HandlerError::fatal("tenant is suspended"));
///         }
///         next.run(ctx)
///     }
/// }
/// ```
pub trait Middleware: Send + Sync {
    /// Runs around the rest of the chain.
    fn call(&self, ctx: &mut JobContext, next: Next<'_>) -> Result<Outcome, HandlerError>;
}

impl<M: Middleware + ?Sized> Middleware for Arc<M> {
    fn call(&self, ctx: &mut JobContext, next: Next<'_>) -> Result<Outcome, HandlerError> {
        (**self).call(ctx, next)
    }
}

/// The rest of the chain after the current middleware.
pub struct Next<'a> {
    chain: &'a [Arc<dyn Middleware>],
    handler: &'a dyn Handler,
}

impl<'a> Next<'a> {
    pub(crate) fn new(chain: &'a [Arc<dyn Middleware>], handler: &'a dyn Handler) -> Next<'a> {
        Next { chain, handler }
    }

    /// Runs the remaining middleware and then the handler.
    pub fn run(self, ctx: &mut JobContext) -> Result<Outcome, HandlerError> {
        match self.chain.split_first() {
            Some((first, rest)) => first.call(ctx, Next::new(rest, self.handler)),
            None => self.handler.handle(ctx),
        }
    }
}

impl fmt::Debug for Next<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Next")
            .field("remaining_middleware", &self.chain.len())
            .finish()
    }
}

/// [`Middleware`] made from a closure by [`middleware_fn`].
#[derive(Clone)]
pub struct FnMiddleware<F>(F);

impl<F> fmt::Debug for FnMiddleware<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FnMiddleware(..)")
    }
}

impl<F> Middleware for FnMiddleware<F>
where
    F: Fn(&mut JobContext, Next<'_>) -> Result<Outcome, HandlerError> + Send + Sync,
{
    fn call(&self, ctx: &mut JobContext, next: Next<'_>) -> Result<Outcome, HandlerError> {
        (self.0)(ctx, next)
    }
}

/// Turns a closure into [`Middleware`].
///
/// ```
/// use belay_worker::middleware::{middleware_fn, Next};
/// use belay_worker::{JobContext, Registry};
///
/// #[derive(Debug)]
/// struct Region(&'static str);
///
/// let registry = Registry::new().wrap(middleware_fn(|ctx: &mut JobContext, next: Next<'_>| {
///     ctx.extensions_mut().insert(Region("eu-west"));
///     next.run(ctx)
/// }));
/// # let _ = registry;
/// ```
pub fn middleware_fn<F>(f: F) -> FnMiddleware<F>
where
    F: Fn(&mut JobContext, Next<'_>) -> Result<Outcome, HandlerError> + Send + Sync,
{
    FnMiddleware(f)
}

/// Logs a line when a job starts and one when it finishes, with its
/// duration and outcome. Failures are logged at `warn` whatever the
/// configured level.
#[derive(Clone, Copy, Debug)]
pub struct LoggingMiddleware {
    level: Level,
}

impl LoggingMiddleware {
    /// Logs at `info`.
    #[must_use]
    pub fn new() -> LoggingMiddleware {
        LoggingMiddleware { level: Level::Info }
    }

    /// Logs starts and successes at `level` instead.
    #[must_use]
    pub fn level(mut self, level: Level) -> LoggingMiddleware {
        self.level = level;
        self
    }
}

impl Default for LoggingMiddleware {
    fn default() -> LoggingMiddleware {
        LoggingMiddleware::new()
    }
}

impl Middleware for LoggingMiddleware {
    fn call(&self, ctx: &mut JobContext, next: Next<'_>) -> Result<Outcome, HandlerError> {
        crate::job_log!(self.level, ctx, "job started"; queue = %ctx.queue());
        let started = Instant::now();
        let result = next.run(ctx);
        let duration = started.elapsed();
        match &result {
            Ok(_) => crate::job_log!(self.level, ctx, "job finished"; duration, outcome = "ok"),
            Err(e) => crate::job_warn!(
                ctx, "job failed";
                duration,
                error = %e,
                retryable = e.retryable,
                error_kind = e.kind.as_deref(),
            ),
        }
        result
    }
}

/// Counters for one job kind, collected by [`TimingMiddleware`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KindTiming {
    /// Attempts that finished (including failures and panics).
    pub runs: u64,
    /// Attempts that failed or panicked.
    pub errors: u64,
    /// Time spent in all attempts.
    pub total: Duration,
    /// The slowest attempt.
    pub max: Duration,
    /// The most recent attempt.
    pub last: Duration,
}

impl KindTiming {
    /// Mean attempt duration.
    #[must_use]
    pub fn mean(&self) -> Duration {
        match u32::try_from(self.runs) {
            Ok(0) => Duration::ZERO,
            Ok(n) => self.total / n,
            Err(_) => Duration::from_secs_f64(self.total.as_secs_f64() / self.runs as f64),
        }
    }

    fn record(&mut self, elapsed: Duration, failed: bool) {
        self.runs += 1;
        self.errors += u64::from(failed);
        self.total = self.total.saturating_add(elapsed);
        self.max = self.max.max(elapsed);
        self.last = elapsed;
    }
}

/// Shared per-kind timing table; clones see the same data.
#[derive(Clone, Debug, Default)]
pub struct Timings {
    kinds: Arc<Mutex<BTreeMap<String, KindTiming>>>,
}

impl Timings {
    /// Counters for every kind seen so far.
    #[must_use]
    pub fn snapshot(&self) -> BTreeMap<String, KindTiming> {
        lock(&self.kinds).clone()
    }

    /// Counters for one kind.
    #[must_use]
    pub fn get(&self, kind: &str) -> Option<KindTiming> {
        lock(&self.kinds).get(kind).copied()
    }

    /// Forgets everything.
    pub fn reset(&self) {
        lock(&self.kinds).clear();
    }

    fn record(&self, kind: &str, elapsed: Duration, failed: bool) {
        let mut kinds = lock(&self.kinds);
        match kinds.get_mut(kind) {
            Some(t) => t.record(elapsed, failed),
            None => {
                let mut t = KindTiming::default();
                t.record(elapsed, failed);
                kinds.insert(kind.to_owned(), t);
            }
        }
    }
}
