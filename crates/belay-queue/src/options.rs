//! Engine settings: [`EngineOptions`] and [`QueueOrder`].
//!
//! Per-queue behaviour (concurrency, rate limits, retries, retention) lives
//! in each queue's [`QueueConfig`](belay_proto::QueueConfig); these options
//! cover what is engine-wide: fairness knobs, batch sizes, worker tracking
//! and the randomness seed that makes tests reproducible.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use belay_proto::TenantId;
use belay_store::Namespace;

/// How a lease request naming several queues spreads its picks over them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum QueueOrder {
    /// Take one job from each queue in turn, starting from a position that
    /// rotates between requests, so that a flooded queue cannot starve the
    /// others. The default. Text form `round_robin`.
    #[default]
    RoundRobin,
    /// Drain the queues in the order the request lists them: the second is
    /// only consulted when the first has nothing to give. Lets workers
    /// express strict preference. Text form `in_order`.
    InOrder,
}

impl fmt::Display for QueueOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            QueueOrder::RoundRobin => "round_robin",
            QueueOrder::InOrder => "in_order",
        })
    }
}

/// Error parsing a [`QueueOrder`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseQueueOrderError(String);

impl fmt::Display for ParseQueueOrderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown queue order {:?}, expected round_robin or in_order",
            self.0
        )
    }
}

impl std::error::Error for ParseQueueOrderError {}

impl FromStr for QueueOrder {
    type Err = ParseQueueOrderError;

    fn from_str(s: &str) -> Result<QueueOrder, ParseQueueOrderError> {
        match s {
            "round_robin" => Ok(QueueOrder::RoundRobin),
            "in_order" => Ok(QueueOrder::InOrder),
            _ => Err(ParseQueueOrderError(s.to_owned())),
        }
    }
}

/// Settings for [`QueueEngine::open`](crate::QueueEngine::open), built with
/// chained methods from [`EngineOptions::new`].
///
/// ```
/// use std::time::Duration;
/// use belay_queue::{EngineOptions, QueueOrder};
///
/// let opts = EngineOptions::new()
///     .seed(7)
///     .queue_order(QueueOrder::InOrder)
///     .tenant_weight("acme".parse().unwrap(), 3)
///     .worker_timeout(Duration::from_secs(120));
/// assert_eq!(opts.tenant_quantum_for(Some(&"acme".parse().unwrap())), 3);
/// assert_eq!(opts.tenant_quantum_for(None), 1);
/// ```
#[derive(Clone, Debug)]
pub struct EngineOptions {
    pub(crate) seed: Option<u64>,
    pub(crate) namespace: Namespace,
    pub(crate) queue_order: QueueOrder,
    pub(crate) tenant_quantum: u32,
    pub(crate) tenant_weights: BTreeMap<TenantId, u32>,
    pub(crate) batch_size: usize,
    pub(crate) retention_per_tick: usize,
    pub(crate) throughput_window: Duration,
    pub(crate) worker_timeout: Duration,
    pub(crate) worker_retention: Duration,
    pub(crate) max_poll_interval: Duration,
    pub(crate) latency_samples: usize,
    pub(crate) open_chunk: usize,
}

impl EngineOptions {
    /// Default number of job changes written per store batch by `tick`,
    /// `purge` and `delete_queue`.
    pub const DEFAULT_BATCH_SIZE: usize = 256;
    /// Default cap on retention deletions per `tick` call.
    pub const DEFAULT_RETENTION_PER_TICK: usize = 10_000;
    /// Default window of the per-queue throughput counters.
    pub const DEFAULT_THROUGHPUT_WINDOW: Duration = Duration::from_secs(60);
    /// Default silence after which a worker is reported lost. Longer than
    /// the longest long-poll wait, so a worker parked in a lease request is
    /// never considered gone.
    pub const DEFAULT_WORKER_TIMEOUT: Duration = Duration::from_secs(120);
    /// Default time a lost worker without jobs stays listed.
    pub const DEFAULT_WORKER_RETENTION: Duration = Duration::from_secs(3600);
    /// Default upper bound on one long-poll sleep between re-checks.
    pub const DEFAULT_MAX_POLL_INTERVAL: Duration = Duration::from_secs(1);
    /// Default capacity of the lease latency sample buffer.
    pub const DEFAULT_LATENCY_SAMPLES: usize = 4096;

    /// Defaults: entropy-seeded randomness, keys at the root of the store,
    /// round-robin over queues, one job per tenant turn, batches of 256.
    #[must_use]
    pub fn new() -> EngineOptions {
        EngineOptions {
            seed: None,
            namespace: Namespace::root(),
            queue_order: QueueOrder::RoundRobin,
            tenant_quantum: 1,
            tenant_weights: BTreeMap::new(),
            batch_size: EngineOptions::DEFAULT_BATCH_SIZE,
            retention_per_tick: EngineOptions::DEFAULT_RETENTION_PER_TICK,
            throughput_window: EngineOptions::DEFAULT_THROUGHPUT_WINDOW,
            worker_timeout: EngineOptions::DEFAULT_WORKER_TIMEOUT,
            worker_retention: EngineOptions::DEFAULT_WORKER_RETENTION,
            max_poll_interval: EngineOptions::DEFAULT_MAX_POLL_INTERVAL,
            latency_samples: EngineOptions::DEFAULT_LATENCY_SAMPLES,
            open_chunk: 1024,
        }
    }

    /// Seeds the engine's random number generator (lease tokens, retry
    /// jitter, job ids), making runs reproducible under a manual clock.
    #[must_use]
    pub fn seed(mut self, seed: u64) -> EngineOptions {
        self.seed = Some(seed);
        self
    }

    /// Keeps every record under `namespace`, so that several engines (or
    /// other subsystems) can share one store.
    #[must_use]
    pub fn namespace(mut self, namespace: Namespace) -> EngineOptions {
        self.namespace = namespace;
        self
    }

    /// Sets how multi-queue lease requests pick between queues.
    #[must_use]
    pub fn queue_order(mut self, order: QueueOrder) -> EngineOptions {
        self.queue_order = order;
        self
    }

    /// Jobs a tenant may take in one turn of the deficit round robin before
    /// the next tenant gets its turn (at least 1). Larger values trade
    /// fairness granularity for fewer switches between tenants.
    #[must_use]
    pub fn tenant_quantum(mut self, quantum: u32) -> EngineOptions {
        self.tenant_quantum = quantum.max(1);
        self
    }

    /// Gives `tenant` `weight` times the base quantum per turn (at least 1),
    /// i.e. that share of the queue while it competes with weight-1
    /// tenants.
    #[must_use]
    pub fn tenant_weight(mut self, tenant: TenantId, weight: u32) -> EngineOptions {
        self.tenant_weights.insert(tenant, weight.max(1));
        self
    }

    /// Job changes per store batch in `tick`, `purge` and `delete_queue`
    /// (at least 1). Bounds how long one batch holds the engine lock.
    #[must_use]
    pub fn batch_size(mut self, n: usize) -> EngineOptions {
        self.batch_size = n.max(1);
        self
    }

    /// Most finished jobs one `tick` deletes for retention (at least 1);
    /// the rest wait for the next tick.
    #[must_use]
    pub fn retention_per_tick(mut self, n: usize) -> EngineOptions {
        self.retention_per_tick = n.max(1);
        self
    }

    /// Window of the throughput counters in queue info (at least 1s).
    #[must_use]
    pub fn throughput_window(mut self, window: Duration) -> EngineOptions {
        self.throughput_window = window.max(Duration::from_secs(1));
        self
    }

    /// Silence after which a worker is reported lost.
    #[must_use]
    pub fn worker_timeout(mut self, timeout: Duration) -> EngineOptions {
        self.worker_timeout = timeout;
        self
    }

    /// How long a lost worker holding no jobs stays in the registry.
    #[must_use]
    pub fn worker_retention(mut self, retention: Duration) -> EngineOptions {
        self.worker_retention = retention;
        self
    }

    /// Longest single sleep of a long-polling lease request before it
    /// re-checks the queues on its own (at least 1ms). Wakeups normally
    /// come from enqueues and ticks; this bounds how late a request notices
    /// a rate-limit token or timer it could not be told about.
    #[must_use]
    pub fn max_poll_interval(mut self, interval: Duration) -> EngineOptions {
        self.max_poll_interval = interval.max(Duration::from_millis(1));
        self
    }

    /// Capacity of the lease latency buffer drained by
    /// [`QueueEngine::drain_lease_latencies`](crate::QueueEngine::drain_lease_latencies).
    #[must_use]
    pub fn latency_samples(mut self, n: usize) -> EngineOptions {
        self.latency_samples = n;
        self
    }

    /// The configured queue order.
    #[must_use]
    pub fn order(&self) -> QueueOrder {
        self.queue_order
    }

    /// The batch size in effect.
    #[must_use]
    pub fn batch(&self) -> usize {
        self.batch_size
    }

    /// Jobs per turn for `tenant` (`None` is the tenant of jobs that name
    /// none): the base quantum times the tenant's weight.
    #[must_use]
    pub fn tenant_quantum_for(&self, tenant: Option<&TenantId>) -> u32 {
        let weight = tenant
            .and_then(|t| self.tenant_weights.get(t))
            .copied()
            .unwrap_or(1);
        self.tenant_quantum.saturating_mul(weight)
    }
}
