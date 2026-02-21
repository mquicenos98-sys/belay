//! Opening an engine: the store, the three engines wired together, the
//! event bus and metrics, the configured queues, calendars and schedules,
//! the cluster node, and finally the background threads.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use belay_cluster::Transport;
use belay_common::{info, system_clock, SharedClock};
use belay_metrics::Registry;
use belay_queue::QueueEngine;
use belay_scheduler::{Scheduler, Upserted, WorkflowEngine};
use belay_store::{Kv, LogKv, MemKv};

use super::{Engine, Shared, StorageKind};
use crate::cluster::Cluster;
use crate::config::Config;
use crate::dispatch::{QueueDispatcher, WorkflowDriver};
use crate::error::{Error, Result};
use crate::events::{BusOptions, EventBus};
use crate::metrics::Metrics;

/// Starts an [`Engine`] with parts replaced, mostly for tests.
///
/// ```
/// use belay_common::{ManualClock, Timestamp};
/// use belay_core::{Config, Engine};
///
/// let clock = ManualClock::new(Timestamp::from_secs(1_700_000_000));
/// let engine = Engine::builder()
///     .config(Config::dev())
///     .clock(clock.shared())
///     .background(false)
///     .start()?;
/// assert_eq!(engine.info().started_at, Timestamp::from_secs(1_700_000_000));
/// # Ok::<(), belay_core::Error>(())
/// ```
#[must_use]
pub struct EngineBuilder {
    config: Config,
    clock: Option<SharedClock>,
    kv: Option<Arc<dyn Kv>>,
    transport: Option<Arc<dyn Transport>>,
    registry: Option<Registry>,
    background: bool,
    seed: Option<u64>,
}

impl std::fmt::Debug for EngineBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineBuilder")
            .field("custom_clock", &self.clock.is_some())
            .field("custom_kv", &self.kv.is_some())
            .field("custom_transport", &self.transport.is_some())
            .field("background", &self.background)
            .field("seed", &self.seed)
            .finish_non_exhaustive()
    }
}

impl Default for EngineBuilder {
    fn default() -> EngineBuilder {
        EngineBuilder::new()
    }
}

impl EngineBuilder {
    /// Defaults: [`Config::default`], the system clock, the store the
    /// configuration describes, background threads on.
    pub fn new() -> EngineBuilder {
        EngineBuilder {
            config: Config::default(),
            clock: None,
            kv: None,
            transport: None,
            registry: None,
            background: true,
            seed: None,
        }
    }

    /// The configuration to run with.
    pub fn config(mut self, config: Config) -> EngineBuilder {
        self.config = config;
        self
    }

    /// The clock every engine reads, e.g. a
    /// [`ManualClock`](belay_common::ManualClock).
    pub fn clock(mut self, clock: SharedClock) -> EngineBuilder {
        self.clock = Some(clock);
        self
    }

    /// A store to use instead of the one `[storage]` describes. The engine
    /// flushes it on shutdown but does not close it.
    pub fn kv(mut self, kv: Arc<dyn Kv>) -> EngineBuilder {
        self.kv = Some(kv);
        self
    }

    /// The datagram transport for cluster gossip (a
    /// [`MemNetwork`](belay_cluster::MemNetwork) endpoint in tests) instead
    /// of a UDP socket on `cluster.gossip_listen`. Only used when
    /// `cluster.enabled`.
    pub fn cluster_transport(mut self, transport: Arc<dyn Transport>) -> EngineBuilder {
        self.transport = Some(transport);
        self
    }

    /// The registry to put metrics in, e.g. one shared with the HTTP
    /// server's own metrics.
    pub fn registry(mut self, registry: Registry) -> EngineBuilder {
        self.registry = Some(registry);
        self
    }

    /// Whether to start the background threads (default true). Without
    /// them nothing happens by itself: call [`Engine::tick`] to run the
    /// periodic duties, which makes tests with a manual clock
    /// deterministic.
    pub fn background(mut self, spawn: bool) -> EngineBuilder {
        self.background = spawn;
        self
    }

    /// Seeds the queue engine's random numbers (lease tokens, retry
    /// jitter) for reproducible runs.
    pub fn seed(mut self, seed: u64) -> EngineBuilder {
        self.seed = Some(seed);
        self
    }

    /// Opens everything and starts the engine.
    pub fn start(self) -> Result<Engine> {
        let EngineBuilder {
            config,
            clock,
            kv,
            transport,
            registry,
            background,
            seed,
        } = self;
        let clock = clock.unwrap_or_else(system_clock);
        let started_at = clock.now();

        let (kv, log_kv, storage) = open_store(&config, kv)?;

        // Sequence numbers start at the start time in seconds times 2^20,
        // so they keep growing across restarts and a subscriber resuming
        // with an id from before a restart is never "ahead" of the stream.
        // Until 2106 they stay below 2^53 and survive JSON number parsing
        // in JavaScript clients.
        let first_seq = u64::try_from(started_at.as_secs()).unwrap_or(0) << 20;
        let events = EventBus::with_options(
            clock.clone(),
            BusOptions {
                capacity: config.events.buffer,
                subscriber_queue: config.events.subscriber_queue,
                first_seq: first_seq.max(1),
            },
        );
        let metrics = Metrics::register(
            registry.unwrap_or_default(),
            config.metrics.enabled,
            config.metrics.process,
            &events,
        )?;

        let mut engine_options = config.engine.engine_options();
        if let Some(seed) = seed {
            engine_options = engine_options.seed(seed);
        }
        let sink = Arc::new(events.clone());
        let queue = Arc::new(QueueEngine::open(
            Arc::clone(&kv),
            clock.clone(),
            sink.clone(),
            engine_options,
        )?);
        let dispatcher = Arc::new(QueueDispatcher::new(Arc::clone(&queue)));
        let scheduler = Arc::new(Scheduler::open_with(
            Arc::clone(&kv),
            clock.clone(),
            dispatcher.clone(),
            sink.clone(),
            config.scheduler.scheduler_options(),
        )?);
        let workflow_engine = Arc::new(WorkflowEngine::open_with(
            Arc::clone(&kv),
            clock.clone(),
            dispatcher,
            sink,
            config.workflows.workflow_options(),
        )?);
        let workflows = Arc::new(WorkflowDriver::new(workflow_engine));
        // The hook holds the driver weakly: the driver reaches the queue
        // through the dispatcher, and a strong reference back would keep
        // the whole engine (and its store's directory lock) alive forever.
        let hook = Arc::downgrade(&workflows);
        queue.on_finished(move |job| {
            if let Some(driver) = hook.upgrade() {
                driver.job_finished(job);
            }
        });

        apply_config(&config, &queue, &scheduler)?;
        let recovered = workflows
            .call(|w| w.recover())
            .map_err(|e| Error::apply("workflow recovery", e))?;

        let cluster = if config.cluster.enabled {
            Some(Cluster::start(&config, transport)?)
        } else {
            None
        };
        let node_id = cluster.as_ref().map_or_else(
            || config.cluster.node_id.clone(),
            |c| c.node_id().to_owned(),
        );

        let shared = Arc::new(Shared {
            clock,
            started_at,
            node_id,
            storage,
            kv,
            log_kv,
            queue,
            scheduler,
            workflows,
            events,
            metrics,
            cluster,
            last_checkpoint: Mutex::new(started_at),
            stopping: AtomicBool::new(false),
            config,
        });
        shared.metrics.attach(&shared)?;

        let engine = Engine::from_parts(Arc::clone(&shared));
        if background {
            engine.spawn_duties()?;
        }
        info!(
            "engine started";
            node = %shared.node_id,
            storage = shared.storage.as_str(),
            clustered = shared.cluster.is_some(),
            workflows_recovered = recovered,
            background
        );
        Ok(engine)
    }
}

type OpenedStore = (Arc<dyn Kv>, Option<Arc<LogKv>>, StorageKind);
