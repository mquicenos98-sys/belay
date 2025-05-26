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

/// How a worker's run ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkerReport {
    /// Statistics at the end.
    pub stats: WorkerStats,
    /// How long the worker ran.
    pub uptime: Duration,
    /// Every running job finished within the shutdown grace period.
    pub drained: bool,
    /// Executor threads still inside a handler when the worker returned.
    pub detached_threads: usize,
}

impl WorkerReport {
    fn from_stats(stats: WorkerStats) -> WorkerReport {
        WorkerReport {
            stats,
            ..WorkerReport::default()
        }
    }
}

impl fmt::Display for WorkerReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Milliseconds are plenty for a summary line.
        let uptime =
            Duration::from_millis(u64::try_from(self.uptime.as_millis()).unwrap_or(u64::MAX));
        write!(
            f,
            "ran for {}, {}",
            belay_common::format_duration(uptime),
            self.stats
        )?;
        if !self.drained {
            f.write_str(", not drained")?;
        }
        if self.detached_threads > 0 {
            write!(f, ", {} handler threads detached", self.detached_threads)?;
        }
        Ok(())
    }
}

/// The worker's threads, as started.
struct Threads {
    started: Instant,
    executors: Vec<JoinHandle<()>>,
    watchdog: Option<JoinHandle<()>>,
    heartbeat: Option<JoinHandle<()>>,
    poller: Option<JoinHandle<()>>,
    stop_heartbeats: Shutdown,
}

impl Threads {
    /// Starts every thread, the poller last so nothing is leased before
    /// the rest is up. On failure, stops what did start.
    fn start(shared: &Arc<Shared>) -> Result<Threads> {
        let mut threads = Threads {
            started: Instant::now(),
            executors: Vec::with_capacity(shared.config.concurrency),
            watchdog: None,
            heartbeat: None,
            poller: None,
            stop_heartbeats: Shutdown::new(),
        };
        if let Err(e) = threads.start_all(shared) {
            threads.abort(shared);
            return Err(e);
        }
        let queues: Vec<&str> = shared.config.queues.iter().map(|q| q.as_str()).collect();
        info!(
            "worker started";
            worker = %shared.config.id,
            queues = queues.join(","),
            concurrency = shared.config.concurrency,
            kinds = shared.registry.kinds().collect::<Vec<_>>().join(","),
        );
        Ok(threads)
    }

    fn start_all(&mut self, shared: &Arc<Shared>) -> Result<()> {
        for i in 0..shared.config.concurrency {
            let s = Arc::clone(shared);
            self.executors
                .push(spawn_named(format!("belay-worker-exec-{i}"), move || {
                    scoped(s.config.logger.as_ref(), || executor::run_executor(&s));
                })?);
        }
        let s = Arc::clone(shared);
        self.watchdog = Some(spawn_named(
            "belay-worker-watchdog".to_owned(),
            move || {
                scoped(s.config.logger.as_ref(), || monitor::run_watchdog(&s));
            },
        )?);
        let s = Arc::clone(shared);
        let stop = self.stop_heartbeats.clone();
        self.heartbeat = Some(spawn_named(
            "belay-worker-heartbeat".to_owned(),
            move || {
                scoped(s.config.logger.as_ref(), || {
                    monitor::run_heartbeats(&s, &stop)
                });
            },
        )?);
        let s = Arc::clone(shared);
        self.poller = Some(spawn_named("belay-worker-poll".to_owned(), move || {
            scoped(s.config.logger.as_ref(), || poller::run_poller(&s));
        })?);
        Ok(())
    }

    /// Stops threads after a failed start. Nothing has been leased yet
    /// (the poller starts last), so there is nothing to drain.
    fn abort(&mut self, shared: &Shared) {
        stop_background(shared, &self.stop_heartbeats);
        close_executors(shared);
        for handle in self.threads_to_join() {
            let _ = handle.join();
        }
    }

    fn threads_to_join(&mut self) -> Vec<JoinHandle<()>> {
        let mut all: Vec<JoinHandle<()>> = self.executors.drain(..).collect();
        all.extend(self.watchdog.take());
        all.extend(self.heartbeat.take());
        all.extend(self.poller.take());
        all
    }
}

/// The shutdown sequence; see [`Worker`].
fn drain(shared: &Shared, mut threads: Threads) -> WorkerReport {
    let config = &shared.config;

    // 1. No more leases; hand back what has not started.
    let (unstarted, held) = {
        let mut state = lock(&shared.state);
        state.accepting = false;
        (state.take_unstarted(), state.jobs.len())
    };
    info!(
        "worker shutting down";
        worker = %config.id,
        running = held.saturating_sub(unstarted.len()),
        unstarted = unstarted.len(),
        grace = config.shutdown_grace,
    );
    shared.draining.trigger();
    shared.changed.notify_all();
    for job in unstarted.iter().filter(|job| job.abandon()) {
        poller::hand_back(shared, job);
    }

    // 2. Let running jobs finish.
    let drained = wait_for_jobs(shared, config.shutdown_grace);

    // 3. Fail whatever is left.
    if !drained {
        let rest: Vec<_> = lock(&shared.state).jobs.values().cloned().collect();
        for job in rest {
            // Abandon before cancelling, so a handler that stops the moment
            // it is cancelled cannot slip in a report of its own first.
            if job.abandon() {
                job.signals.cancel(CancelReason::ShuttingDown);
                Counters::bump(&shared.counters.abandoned);
                warn!(
                    "job still running after the shutdown grace period; handing it back";
                    job = %job.id(),
                    kind = job.kind(),
                );
                poller::hand_back(shared, &job);
            }
        }
        // Jobs whose handler returned meanwhile are being reported.
        wait_for_jobs(shared, config.cancel_grace);
    }

    // 4. Stop the other threads. The poller may be inside a long poll; it
    // hands back whatever that returns.
    if let Some(poller) = threads.poller.take() {
        let _ = poller.join();
    }
    stop_background(shared, &threads.stop_heartbeats);
    for handle in [threads.heartbeat.take(), threads.watchdog.take()]
        .into_iter()
        .flatten()
    {
        let _ = handle.join();
    }
    let detached_threads =
        close_executors_within(shared, &mut threads.executors, config.cancel_grace);

    let report = WorkerReport {
        stats: shared.stats(),
        uptime: threads.started.elapsed(),
        drained,
        detached_threads,
    };
    info!("worker stopped"; worker = %config.id, summary = %report);
    report
}

/// Waits up to `limit` for the job table to empty; `true` if it did.
fn wait_for_jobs(shared: &Shared, limit: Duration) -> bool {
    let deadline = Instant::now().checked_add(limit);
    let mut state = lock(&shared.state);
    loop {
        if state.jobs.is_empty() {
            return true;
        }
        let remaining = match deadline {
            Some(d) => d.saturating_duration_since(Instant::now()),
            None => Duration::from_secs(3600),
        };
        if remaining.is_zero() {
            return false;
        }
        state = wait_timeout(&shared.changed, state, remaining).0;
    }
}

/// Stops the heartbeat and watchdog threads.
fn stop_background(shared: &Shared, stop_heartbeats: &Shutdown) {
    {
        let mut state = lock(&shared.state);
        state.accepting = false;
        state.stopping = true;
    }
    shared.draining.trigger();
    stop_heartbeats.trigger();
    shared.changed.notify_all();
}
