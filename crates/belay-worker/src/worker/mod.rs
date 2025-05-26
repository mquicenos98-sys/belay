//! The [`Worker`]: threads, their life cycle, and shutdown.
//!
//! A running worker has one poller thread (leasing), `concurrency`
//! executor threads (handlers), a heartbeat thread and a watchdog thread.
//! [`Worker::run`] blocks the caller until the given [`Shutdown`]
//! triggers; [`Worker::spawn`] does the same on a supervisor thread and
//! returns a [`WorkerHandle`]. The shutdown steps are documented on
//! [`Worker`].

mod config;

mod executor;

mod monitor;

mod poller;

mod report;

mod state;

use std::fmt;
use std::io;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use belay_common::log::{with_logger, Logger};
use belay_common::sync::{lock, wait_timeout, Shutdown};
use belay_common::{info, warn};
use belay_proto::source::JobSource;
use belay_proto::WorkerId;

use crate::context::CancelReason;
use crate::error::{Error, Result};
use crate::registry::Registry;
use crate::stats::{Counters, WorkerStats};

pub use config::{default_worker_id, WorkerConfig, MAX_CONCURRENCY};

use state::Shared;

/// Leases jobs from a [`JobSource`] and runs them with the handlers in a
/// [`Registry`].
///
/// # Shutdown sequence
///
/// 1. Leasing stops. Jobs that were leased but have not started are failed
///    back at once as retryable "worker shutting down" with `retry_in: 0`,
///    so another worker can take them; a lease call still in flight is
///    waited for and anything it returns is handed back the same way.
/// 2. Running jobs get `shutdown_grace` to finish, with heartbeats keeping
///    their leases alive. Handlers can see
///    [`JobContext::is_shutting_down`](crate::JobContext::is_shutting_down).
/// 3. Jobs still running are cancelled
///    ([`CancelReason::ShuttingDown`](crate::CancelReason::ShuttingDown)),
///    failed back like the unstarted ones, and abandoned.
/// 4. Executor threads get `cancel_grace` to return; threads still inside a
///    handler after that are detached (they cannot be killed) and counted
///    in [`WorkerReport::detached_threads`].
///
/// A handed-back job has used up an attempt (the source counted it when it
/// was leased), and the protocol has no call that returns a lease without
/// a failure; a job on its last attempt therefore ends up dead.
///
/// # Example
///
/// ```
/// use std::sync::Arc;
/// use std::time::Duration;
/// use belay_json::json;
/// use belay_proto::JobSpec;
/// use belay_worker::testing::MemorySource;
/// use belay_worker::{handler_fn, JobContext, Outcome, Registry, Worker, WorkerConfig};
///
/// let source = Arc::new(MemorySource::new());
/// source.push(JobSpec::new("default", "add").payload(json!([2, 3])));
///
/// let registry = Registry::new().register("add", handler_fn(|ctx: &mut JobContext| {
///     let (a, b): (i64, i64) = ctx.payload_as()?;
///     Ok(Outcome::with(a + b))
/// }));
/// let config = WorkerConfig::default().poll_wait(Duration::from_millis(20));
/// let handle = Worker::new(source.clone(), registry, config)?.spawn()?;
///
/// assert!(source.wait_until(Duration::from_secs(10), |s| s.completed() == 1));
/// let report = handle.stop();
/// assert_eq!(report.stats.succeeded, 1);
/// # Ok::<(), belay_worker::Error>(())
/// ```
pub struct Worker {
    shared: Arc<Shared>,
}

impl Worker {
    /// Checks the configuration and the registry. Nothing starts until
    /// [`run`](Self::run) or [`spawn`](Self::spawn).
    pub fn new(
        source: Arc<dyn JobSource>,
        registry: Registry,
        config: WorkerConfig,
    ) -> Result<Worker> {
        config.validate()?;
        registry.validate()?;
        Ok(Worker {
            shared: Arc::new(Shared::new(source, registry, config)),
        })
    }

    /// The worker's id.
    #[must_use]
    pub fn id(&self) -> &WorkerId {
        &self.shared.config.id
    }

    /// The configuration in effect.
    #[must_use]
    pub fn config(&self) -> &WorkerConfig {
        &self.shared.config
    }

    /// Current statistics.
    #[must_use]
    pub fn stats(&self) -> WorkerStats {
        self.shared.stats()
    }

    /// A handle for reading statistics from elsewhere, e.g. a metrics
    /// endpoint, while [`run`](Self::run) blocks.
    #[must_use]
    pub fn stats_handle(&self) -> StatsHandle {
        StatsHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Runs until `shutdown` triggers, then shuts down as described in the
    /// [shutdown sequence](Worker#shutdown-sequence) and reports. Fails
    /// only if a thread cannot be started, in which case nothing has been
    /// leased.
    pub fn run(self, shutdown: Shutdown) -> Result<WorkerReport> {
        let shared = self.shared;
        scoped(shared.config.logger.as_ref(), || {
            let threads = Threads::start(&shared)?;
            shutdown.wait();
            Ok(drain(&shared, threads))
        })
    }

    /// Runs on a supervisor thread; stop it through the returned handle.
    pub fn spawn(self) -> Result<WorkerHandle> {
        let shutdown = Shutdown::new();
        let signal = shutdown.clone();
        let shared = Arc::clone(&self.shared);
        let (started_tx, started_rx) = mpsc::channel::<Result<()>>();
        let thread = spawn_named("belay-worker".to_owned(), move || {
            scoped(shared.config.logger.as_ref(), || {
                let threads = match Threads::start(&shared) {
                    Ok(threads) => {
                        let _ = started_tx.send(Ok(()));
                        threads
                    }
                    Err(e) => {
                        let _ = started_tx.send(Err(e));
                        return None;
                    }
                };
                signal.wait();
                Some(drain(&shared, threads))
            })
        })?;
        // The supervisor always sends before it can finish; a closed channel
        // means it panicked, which `join` would re-raise.
        match started_rx.recv() {
            Ok(Ok(())) => Ok(WorkerHandle {
                shutdown,
                stats: self.stats_handle(),
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => match thread.join() {
                Err(panic) => std::panic::resume_unwind(panic),
                Ok(_) => Err(Error::Spawn {
                    thread: "belay-worker".to_owned(),
                    source: io::Error::new(io::ErrorKind::Other, "supervisor exited early"),
                }),
            },
        }
    }
}

impl fmt::Debug for Worker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Worker")
            .field("config", &self.shared.config)
            .field("registry", &self.shared.registry)
            .finish()
    }
}

/// Reads a worker's statistics; clones share the worker.
#[derive(Clone)]
pub struct StatsHandle {
    shared: Arc<Shared>,
}

impl StatsHandle {
    /// Current statistics.
    #[must_use]
    pub fn snapshot(&self) -> WorkerStats {
        self.shared.stats()
    }
}

impl fmt::Debug for StatsHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StatsHandle")
            .field("worker", &self.shared.config.id)
            .finish()
    }
}

/// Controls a worker started with [`Worker::spawn`].
///
/// Dropping the handle begins a shutdown without waiting for it; call
/// [`stop`](Self::stop) or [`join`](Self::join) to wait and get the report.
pub struct WorkerHandle {
    shutdown: Shutdown,
    stats: StatsHandle,
    thread: Option<JoinHandle<Option<WorkerReport>>>,
}

impl WorkerHandle {
    /// Current statistics.
    #[must_use]
    pub fn stats(&self) -> WorkerStats {
        self.stats.snapshot()
    }

    /// A handle for reading statistics that outlives this one.
    #[must_use]
    pub fn stats_handle(&self) -> StatsHandle {
        self.stats.clone()
    }

    /// Begins a graceful shutdown and returns at once.
    pub fn shutdown(&self) {
        self.shutdown.trigger();
    }

    /// Whether the worker has stopped.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().map_or(true, JoinHandle::is_finished)
    }

    /// Waits for the worker to stop (after [`shutdown`](Self::shutdown),
    /// from this or another handle clone of the signal) and returns its
    /// report.
    ///
    /// # Panics
    ///
    /// Re-raises a panic of the supervisor thread, which would be a bug in
    /// this crate; handler panics never get there.
    #[must_use]
    pub fn join(mut self) -> WorkerReport {
        let thread = self
            .thread
            .take()
            .expect("the join handle is only taken here, and join consumes self");
        match thread.join() {
            Ok(Some(report)) => report,
            // Unreachable: `spawn` only returns a handle once the threads
            // started, and only a failed start yields `None`.
            Ok(None) => WorkerReport::from_stats(self.stats.snapshot()),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    /// [`shutdown`](Self::shutdown), then [`join`](Self::join).
    #[must_use]
    pub fn stop(self) -> WorkerReport {
        self.shutdown();
        self.join()
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.shutdown.trigger();
    }
}

impl fmt::Debug for WorkerHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkerHandle")
            .field("shutdown", &self.shutdown)
            .field("finished", &self.is_finished())
            .finish()
    }
}
