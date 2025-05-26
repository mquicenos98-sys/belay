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

/// Measures every attempt per kind.
///
/// ```
/// use belay_worker::middleware::TimingMiddleware;
/// use belay_worker::{handler_fn, JobContext, Outcome, Registry};
///
/// let timing = TimingMiddleware::new();
/// let timings = timing.timings();
/// let registry = Registry::new()
///     .register("noop", handler_fn(|_: &mut JobContext| Ok(Outcome::done())))
///     .wrap(timing);
/// // ... run a worker with `registry`, then:
/// for (kind, t) in timings.snapshot() {
///     println!("{kind}: {} runs, mean {:?}", t.runs, t.mean());
/// }
/// # let _ = registry;
/// ```
#[derive(Clone, Debug, Default)]
pub struct TimingMiddleware {
    timings: Timings,
}

impl TimingMiddleware {
    /// A middleware with an empty table.
    #[must_use]
    pub fn new() -> TimingMiddleware {
        TimingMiddleware::default()
    }

    /// A handle on the table this middleware fills.
    #[must_use]
    pub fn timings(&self) -> Timings {
        self.timings.clone()
    }
}

impl Middleware for TimingMiddleware {
    fn call(&self, ctx: &mut JobContext, next: Next<'_>) -> Result<Outcome, HandlerError> {
        /// Records on drop, so a panicking handler still counts, as an
        /// error, while the panic unwinds to the executor.
        struct Record<'a> {
            timings: &'a Timings,
            kind: String,
            started: Instant,
            failed: bool,
        }

        impl Drop for Record<'_> {
            fn drop(&mut self) {
                let failed = self.failed || std::thread::panicking();
                self.timings
                    .record(&self.kind, self.started.elapsed(), failed);
            }
        }

        let mut record = Record {
            timings: &self.timings,
            kind: ctx.kind().to_owned(),
            started: Instant::now(),
            failed: false,
        };
        let result = next.run(ctx);
        record.failed = result.is_err();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::handler_fn;
    use crate::registry::Registry;
    use belay_common::log::CaptureLogger;
    use belay_common::Timestamp;
    use belay_proto::{Job, JobId, JobSpec};
    use std::panic::{self, AssertUnwindSafe};

    fn ctx(kind: &str) -> JobContext {
        JobContext::detached(Job::new(
            JobId::generate(),
            JobSpec::new("default", kind),
            Timestamp::from_secs(1),
        ))
    }

    fn tag(name: &'static str, log: Arc<Mutex<Vec<String>>>) -> impl Middleware {
        middleware_fn(move |ctx: &mut JobContext, next: Next<'_>| {
            lock(&log).push(format!("{name}:before"));
            let result = next.run(ctx);
            lock(&log).push(format!("{name}:after"));
            result
        })
    }

    #[test]
    fn first_added_middleware_runs_outermost() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let inner = Arc::clone(&log);
        let registry = Registry::new()
            .register(
                "k",
                handler_fn(move |_: &mut JobContext| {
                    lock(&inner).push("handler".to_owned());
                    Ok(Outcome::done())
                }),
            )
            .wrap(tag("outer", Arc::clone(&log)))
            .wrap(tag("inner", Arc::clone(&log)));
        registry.run(&mut ctx("k")).expect("handler succeeds");
        assert_eq!(
            *lock(&log),
            [
                "outer:before",
                "inner:before",
                "handler",
                "inner:after",
                "outer:after"
            ]
        );
    }

    #[test]
    fn middleware_can_short_circuit_and_pass_extensions() {
        struct Budget(u32);
        let registry = Registry::new()
            .register(
                "k",
                handler_fn(|ctx: &mut JobContext| {
                    let budget = ctx.extensions().get::<Budget>().map_or(0, |b| b.0);
                    Ok(Outcome::with(budget))
                }),
            )
            .wrap(middleware_fn(|ctx: &mut JobContext, next: Next<'_>| {
                if ctx.payload().is_null() {
                    ctx.extensions_mut().insert(Budget(7));
                    next.run(ctx)
                } else {
                    Err(HandlerError::fatal("payloads are not accepted"))
                }
            }));
        assert_eq!(registry.run(&mut ctx("k")), Ok(Outcome::with(7u32)));
    }

    #[test]
    fn logging_middleware_logs_start_and_outcome() {
        let registry = Registry::new()
            .register("ok", handler_fn(|_: &mut JobContext| Ok(Outcome::done())))
            .register(
                "bad",
                handler_fn(|_: &mut JobContext| {
                    Err(HandlerError::fatal("nope").with_kind("bad_input"))
                }),
            )
            .wrap(LoggingMiddleware::new().level(Level::Debug));
        let capture = CaptureLogger::new();
        capture.scope(|| {
            registry.run(&mut ctx("ok")).expect("ok handler");
            registry.run(&mut ctx("bad")).expect_err("bad handler");
        });
        let records = capture.records();
        let summary: Vec<(Level, &str)> = records
            .iter()
            .map(|r| (r.level, r.message.as_str()))
            .collect();
        assert_eq!(
            summary,
            [
                (Level::Debug, "job started"),
                (Level::Debug, "job finished"),
                (Level::Debug, "job started"),
                (Level::Warn, "job failed"),
            ]
        );
        assert_eq!(records[0].field("queue"), Some("default"));
        assert!(records[1].field("duration").is_some());
        assert_eq!(records[3].field("error"), Some("nope"));
        assert_eq!(records[3].field("retryable"), Some("false"));
        assert_eq!(records[3].field("error_kind"), Some("bad_input"));
    }

    #[test]
    fn timing_counts_runs_errors_and_panics_per_kind() {
        let timing = TimingMiddleware::new();
        let timings = timing.timings();
        let registry = Registry::new()
            .register("ok", handler_fn(|_: &mut JobContext| Ok(Outcome::done())))
            .register("err", handler_fn(|_: &mut JobContext| Err("flaky".into())))
            .register(
                "boom",
                handler_fn(|_: &mut JobContext| panic!("handler bug")),
            )
            .wrap(timing);
        registry.run(&mut ctx("ok")).expect("ok handler");
        registry.run(&mut ctx("ok")).expect("ok handler");
        registry.run(&mut ctx("err")).expect_err("err handler");
        let panicked = panic::catch_unwind(AssertUnwindSafe(|| registry.run(&mut ctx("boom"))));
        assert!(panicked.is_err());

        let ok = timings.get("ok").expect("ok ran");
        assert_eq!((ok.runs, ok.errors), (2, 0));
        assert!(ok.max >= ok.last && ok.total >= ok.max);
        assert_eq!(timings.get("err").map(|t| (t.runs, t.errors)), Some((1, 1)));
        assert_eq!(
            timings.get("boom").map(|t| (t.runs, t.errors)),
            Some((1, 1))
        );
        assert_eq!(timings.snapshot().len(), 3);
        timings.reset();
        assert!(timings.snapshot().is_empty());
    }

    #[test]
    fn mean_divides_total_by_runs() {
        let mut t = KindTiming::default();
        assert_eq!(t.mean(), Duration::ZERO);
        t.record(Duration::from_millis(10), false);
        t.record(Duration::from_millis(30), true);
        assert_eq!(t.mean(), Duration::from_millis(20));
        assert_eq!(
            (t.max, t.last, t.errors),
            (Duration::from_millis(30), Duration::from_millis(30), 1)
        );
    }
}
