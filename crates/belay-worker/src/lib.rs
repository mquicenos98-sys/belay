//! The belay worker runtime: leases jobs from a
//! [`JobSource`](belay_proto::source::JobSource) (the embedded engine or
//! the HTTP client), runs the handler registered for each job's kind, and
//! reports the outcome, keeping leases alive with heartbeats meanwhile.
//!
//! | piece | role |
//! |---|---|
//! | [`Handler`], [`handler_fn`] | the code a job kind runs; returns [`Outcome`] or [`HandlerError`] |
//! | [`JobContext`] | the job, its attempt and deadline, cancellation, progress, [`Extensions`] |
//! | [`Registry`], [`KindOptions`] | kind → handler, with per-kind concurrency and timeout |
//! | [`Middleware`], [`middleware`] | code around every handler: logging, timing, your own |
//! | [`Worker`], [`WorkerConfig`] | the threads: poller, executor, heartbeats, watchdog |
//! | [`WorkerStats`], [`WorkerReport`] | what a worker has done |
//! | [`builtin`] | shell, echo, sleep and fail handlers |
//! | [`testing`] | [`MemorySource`](testing::MemorySource), an in-memory source with fault injection |
//!
//! ```
//! use std::sync::Arc;
//! use std::time::Duration;
//! use belay_json::json;
//! use belay_proto::JobSpec;
//! use belay_worker::middleware::LoggingMiddleware;
//! use belay_worker::testing::MemorySource;
//! use belay_worker::{handler_fn, job_info, HandlerError, JobContext, KindOptions, Outcome, Registry, Worker, WorkerConfig};
//!
//! let source = Arc::new(MemorySource::new());
//! let id = source.push(JobSpec::new("emails", "send_welcome").payload(json!({"user": 42})));
//!
//! let registry = Registry::new()
//!     .register_with(
//!         "send_welcome",
//!         handler_fn(|ctx: &mut JobContext| {
//!             let user = ctx.payload()["user"]
//!                 .as_u64()
//!                 .ok_or_else(|| HandlerError::fatal("payload has no user id"))?;
//!             job_info!(ctx, "sending welcome mail"; user);
//!             ctx.progress(50.0, "rendered");
//!             Ok(Outcome::with(json!({"sent_to": user})))
//!         }),
//!         KindOptions::new().concurrency(2).timeout(Duration::from_secs(30)),
//!     )
//!     .wrap(LoggingMiddleware::new());
//!
//! let config = WorkerConfig::default()
//!     .queues(vec!["emails".parse()?])
//!     .poll_wait(Duration::from_millis(20));
//! let handle = Worker::new(source.clone(), registry, config)?.spawn()?;
//! assert!(source.wait_until(Duration::from_secs(10), |s| s.completed() == 1));
//!
//! let report = handle.stop();
//! assert_eq!(report.stats.succeeded, 1);
//! assert!(report.drained);
//! # let _ = id;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Guarantees and decisions
//!
//! - **At-least-once.** A job can run more than once: after a lost lease,
//!   a timeout past the grace period, a result that could not be reported,
//!   or a worker crash. Handlers should be idempotent.
//! - **Leasing.** The worker only asks for as many jobs as it has free
//!   executor slots, so it never holds more leases than it has threads.
//!   When every registered kind has a [`KindOptions::concurrency`] limit,
//!   it asks for no more than those limits leave room for. Lease requests
//!   cannot filter by kind, so a job whose kind is at its limit waits on
//!   the worker (heartbeated, holding a slot) until a job of its kind
//!   finishes; while one waits the worker asks for nothing more. Give kinds
//!   with tight limits their own queue if they share one with high-volume
//!   work.
//! - **Timeouts.** An attempt's budget is the shorter of the job's
//!   `timeout` and the kind's; at the deadline the context is cancelled,
//!   and a handler still running [`cancel_grace`](WorkerConfig::cancel_grace)
//!   later is reported as a retryable `timeout` failure and abandoned.
//!   Threads cannot be killed: an abandoned handler keeps its executor
//!   slot until it returns, and its result is dropped.
//! - **Panics** are caught and reported as retryable failures of kind
//!   `panic`, subject to the job's remaining attempts. A panic is often
//!   caused by the input and will recur, but it can as easily come from a
//!   transient condition (a poisoned connection, an unexpected upstream
//!   answer) — and a job that keeps panicking still ends dead once its
//!   attempts run out, with the panic messages in its error history.
//! - **Unknown kinds** fail at once, non-retryably, with `no handler
//!   registered for kind "x"`.
//! - **Shutdown** stops leasing, hands back jobs that have not started,
//!   waits [`shutdown_grace`](WorkerConfig::shutdown_grace) for running
//!   ones and then fails the rest as retryable "worker shutting down" with
//!   `retry_in: 0`; see [`Worker`](Worker#shutdown-sequence) for the steps.
//!
//! # Limitations
//!
//! - Handler threads cannot be killed; a handler that ignores cancellation
//!   costs a slot until it returns and is detached at shutdown.
//! - A job handed back at shutdown has used up an attempt; the protocol
//!   cannot return a lease without a failure.
//! - The shell handler kills only the direct child process.
//! - Heartbeats are one call per job per period; the protocol has no batch
//!   heartbeat.
//! - Deadlines use the worker's monotonic clock from the moment the handler
//!   starts; time spent waiting for a slot does not count.

#![forbid(unsafe_code)]
#![warn(missing_docs, missing_debug_implementations)]

pub mod builtin;

mod context;

mod error;

mod extensions;

mod handler;

pub mod macros;

pub mod middleware;

mod registry;
