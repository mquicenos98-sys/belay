//! [`Engine`]: storage, queue, scheduler, workflows, events, metrics and
//! clustering, started together and driven by background threads.
//!
//! The engine is split by concern: opening everything and applying the
//! configuration (`builder`), the periodic duties and their threads
//! (`duties`), the request-facing methods (`api`) and the worker protocol
//! (`source`). How the parts are wired is described on [`Engine`].

mod api;
mod builder;
mod duties;
mod source;

pub use api::{Paging, StoreReport, VerifyOutcome};
pub use builder::EngineBuilder;
pub use duties::TickReport;

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use belay_common::sync::lock;
use belay_common::{info, warn, SharedClock, Shutdown, Timestamp};
use belay_metrics::Registry;
use belay_proto::api::ServerInfo;
use belay_proto::event::EventFilter;
use belay_queue::QueueEngine;
use belay_scheduler::Scheduler;
use belay_store::{Kv, LogKv};

use crate::cluster::Cluster;
use crate::config::Config;
use crate::dispatch::WorkflowDriver;
use crate::events::{EventBus, Subscription};
use crate::metrics::Metrics;

/// How the engine's store was provided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StorageKind {
    /// `storage.in_memory`.
    Memory,
    /// A log-structured store in `storage.dir`.
    Disk,
    /// A store handed to the builder.
    Custom,
}

impl StorageKind {
    fn as_str(self) -> &'static str {
        match self {
            StorageKind::Memory => "memory",
            StorageKind::Disk => "disk",
            StorageKind::Custom => "custom",
        }
    }
}

/// Everything the engine's threads and handles share.
pub(crate) struct Shared {
    pub(crate) config: Config,
    pub(crate) clock: SharedClock,
    pub(crate) started_at: Timestamp,
    pub(crate) node_id: String,
    pub(crate) storage: StorageKind,
    pub(crate) kv: Arc<dyn Kv>,
    /// The same store as `kv` when the engine opened a [`LogKv`], kept
    /// typed for the store report.
    pub(crate) log_kv: Option<Arc<LogKv>>,
    pub(crate) queue: Arc<QueueEngine>,
    pub(crate) scheduler: Arc<Scheduler>,
    pub(crate) workflows: Arc<WorkflowDriver>,
    pub(crate) events: EventBus,
    pub(crate) metrics: Metrics,
    pub(crate) cluster: Option<Cluster>,
    pub(crate) last_checkpoint: Mutex<Timestamp>,
    pub(crate) stopping: AtomicBool,
}

impl Shared {
    pub(crate) fn uptime(&self) -> Duration {
        self.clock.now().saturating_duration_since(self.started_at)
    }

    /// Members by state name, for metrics; a single node counts itself.
    pub(crate) fn member_counts(&self) -> Vec<(&'static str, usize)> {
        let Some(cluster) = &self.cluster else {
            return vec![("alive", 1)];
        };
        let mut counts: Vec<(&'static str, usize)> = Vec::new();
        for member in cluster.members() {
            let state = member.state.as_str();
            match counts.iter_mut().find(|(s, _)| *s == state) {
                Some((_, n)) => *n += 1,
                None => counts.push((state, 1)),
            }
        }
        counts
    }
}

/// The belay engine. Share it as `Arc<Engine>`; every method takes
/// `&self`.
///
/// ```
/// use belay_core::{Config, Engine};
/// use belay_proto::api::{CompleteRequest, LeaseRequest};
/// use belay_proto::{JobSource, JobSpec, JobState, QueueName, WorkerId};
///
/// let engine = Engine::builder().config(Config::dev()).background(false).start()?;
/// let id = engine.enqueue(JobSpec::new("emails", "send_welcome"))?.id;
///
/// let req = LeaseRequest::new(WorkerId::from_static("w1"), vec![QueueName::from_static("emails")]);
/// let leased = engine.lease(&req)?;
/// engine.complete(&id, &CompleteRequest::new(leased[0].token))?;
/// assert_eq!(engine.get_job(&id)?.state, JobState::Succeeded);
/// engine.shutdown();
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Wiring
///
/// ```text
///   API calls ──────► QueueEngine ◄──── dispatch ──── Scheduler
///                        │   ▲                        WorkflowEngine
///            on_finished │   └──── dispatch ────┐           ▲
///                        ▼                      │           │ on_job_finished
///                  WorkflowDriver ──────────────┴───────────┘
///
///   all three ── emit ──► EventBus ──► subscriptions, metric counters
/// ```
///
/// The scheduler and the workflow engine create jobs only through a
/// dispatcher over the queue, which honours their deduplication keys; the
/// queue reports every finished job back, and the workflow driver hands
/// the ones that belong to workflow runs to the workflow engine.
///
/// # Shutdown
///
/// [`Engine::shutdown`] (also run on drop) stops the background threads,
/// leaves the cluster, stops the queue from handing out jobs (which wakes
/// long-polling lease requests), closes the event bus, and finally flushes
/// and closes the store. Calls that need the store fail with `unavailable`
/// from then on.
pub struct Engine {
    shared: Arc<Shared>,
    shutdown: Shutdown,
    threads: Mutex<Vec<JoinHandle<()>>>,
    stopped: AtomicBool,
}

impl fmt::Debug for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Engine")
            .field("node", &self.shared.node_id)
            .field("storage", &self.shared.storage)
            .field("clustered", &self.shared.cluster.is_some())
            .field("stopped", &self.stopped.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// Starts an engine from `config` with the system clock and background
    /// threads: what `belay server` does.
    pub fn start(config: Config) -> crate::Result<Engine> {
        Engine::builder().config(config).start()
    }

    /// A builder for engines with an injected clock, store or cluster
    /// transport, or without background threads (tests drive them with
    /// [`tick`](Self::tick)).
    #[must_use]
    pub fn builder() -> EngineBuilder {
        EngineBuilder::new()
    }

    /// The configuration the engine runs with.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.shared.config
    }

    /// The engine's clock.
    #[must_use]
    pub fn clock(&self) -> &SharedClock {
        &self.shared.clock
    }

    /// This node's id: `cluster.node_id`.
    #[must_use]
    pub fn node_id(&self) -> &str {
        &self.shared.node_id
    }

    /// Time since the engine started, by its clock.
    #[must_use]
    pub fn uptime(&self) -> Duration {
        self.shared.uptime()
    }

    /// What `GET /v1/info` returns.
    #[must_use]
    pub fn info(&self) -> ServerInfo {
        ServerInfo {
            version: crate::VERSION.to_owned(),
            node: self.shared.node_id.clone(),
            started_at: self.shared.started_at,
            uptime: self.shared.uptime(),
            time: self.shared.clock.now(),
            storage: self.shared.storage.as_str().to_owned(),
            cluster_size: self
                .shared
                .cluster
                .as_ref()
                .map_or(1, |c| u32::try_from(c.live_count()).unwrap_or(u32::MAX)),
        }
    }

    /// Whether the engine should receive traffic (`/readyz`): it is not
    /// shutting down and, when clustered, it has joined (or waited out its
    /// join grace period).
    #[must_use]
    pub fn is_ready(&self) -> bool {
        !self.shared.stopping.load(Ordering::SeqCst)
            && self.shared.cluster.as_ref().map_or(true, Cluster::may_fire)
    }

    /// Whether [`shutdown`](Self::shutdown) has begun.
    #[must_use]
    pub fn is_shutting_down(&self) -> bool {
        self.shared.stopping.load(Ordering::SeqCst)
    }

    /// The metrics registry (`GET /metrics`). Render it with
    /// [`render_metrics`](Self::render_metrics) to include the latest
    /// lease latencies.
    #[must_use]
    pub fn metrics(&self) -> &Registry {
        self.shared.metrics.registry()
    }

    /// Folds pending lease latency samples into the histogram, then
    /// renders the registry in the Prometheus text format.
    #[must_use]
    pub fn render_metrics(&self) -> String {
        self.shared.refresh_metrics();
        self.shared.metrics.registry().render_prometheus()
    }

    /// The event bus.
    #[must_use]
    pub fn events(&self) -> &EventBus {
        &self.shared.events
    }

    /// Subscribes to the event stream (`GET /v1/events`); see
    /// [`EventBus::subscribe`].
    #[must_use]
    pub fn subscribe(&self, filter: EventFilter) -> Subscription {
        self.shared.events.subscribe(filter)
    }

    /// Stops the engine in the order described under
    /// [Shutdown](Engine#shutdown). Idempotent, and run automatically on
    /// drop.
    pub fn shutdown(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        let shared = &self.shared;
        shared.stopping.store(true, Ordering::SeqCst);
        self.shutdown.trigger();
        for handle in std::mem::take(&mut *lock(&self.threads)) {
            if handle.join().is_err() {
                warn!("an engine thread panicked");
            }
        }
        if let Some(cluster) = &shared.cluster {
            cluster.leave();
        }
        shared.queue.shutdown();
        shared.events.close();
        if let Err(e) = shared.kv.flush() {
            warn!("flushing the store failed"; error = %e);
        }
        info!("engine stopped"; node = %shared.node_id, uptime = shared.uptime());
    }

    fn from_parts(shared: Arc<Shared>) -> Engine {
        Engine {
            shared,
            shutdown: Shutdown::new(),
            threads: Mutex::new(Vec::new()),
            stopped: AtomicBool::new(false),
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn engines_can_be_shared_between_threads() {
        // The HTTP server hands one Arc<Engine> to every request thread.
        assert_send_sync::<Engine>();
        assert_send_sync::<Shared>();
    }

    #[test]
    fn storage_kinds_have_stable_names() {
        let names: Vec<&str> = [StorageKind::Memory, StorageKind::Disk, StorageKind::Custom]
            .iter()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(names, ["memory", "disk", "custom"]);
    }
}
