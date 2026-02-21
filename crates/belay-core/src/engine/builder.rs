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
