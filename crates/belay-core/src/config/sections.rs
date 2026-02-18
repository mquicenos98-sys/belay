//! The plain sections of the configuration: everything except `[auth]`
//! (see `auth`) and the domain objects (`[queues.*]`, `[[schedules]]`,
//! `[[calendars]]`), each with its defaults and how it is read.

use std::collections::BTreeMap;
use std::fmt;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use belay_common::log::{Filter, LogFormat};
use belay_conf::{Reader, Result};
use belay_proto::TenantId;
use belay_queue::{EngineOptions, QueueOrder};
use belay_scheduler::{SchedulerOptions, WorkflowOptions};
use belay_store::{Options, RecoveryMode, SyncPolicy};

use super::read::{bytes, duration};

const MIB: u64 = 1 << 20;

/// `[server]`: how the HTTP API is served.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerConfig {
    /// Address to listen on, `host:port`. Default `127.0.0.1:7700`.
    pub listen: String,
    /// Request handling threads. Default 16.
    pub threads: usize,
    /// Most open connections; more are refused. Default 1024.
    pub max_connections: usize,
    /// Largest accepted request body in bytes. Default 4 MiB.
    pub max_body: u64,
    /// How long an idle keep-alive connection stays open. Default 60s.
    pub keep_alive: Duration,
    /// Budget for reading one request. Default 30s.
    pub request_timeout: Duration,
    /// How long shutdown waits for requests in flight. Default 30s.
    pub shutdown_grace: Duration,
}

impl Default for ServerConfig {
    fn default() -> ServerConfig {
        ServerConfig {
            listen: "127.0.0.1:7700".to_owned(),
            threads: 16,
            max_connections: 1024,
            max_body: 4 * MIB,
            keep_alive: Duration::from_secs(60),
            request_timeout: Duration::from_secs(30),
            shutdown_grace: Duration::from_secs(30),
        }
    }
}

impl ServerConfig {
    pub(crate) fn read(r: &Reader<'_>) -> Result<ServerConfig> {
        let d = ServerConfig::default();
        let config = ServerConfig {
            listen: r.string_or("server.listen", &d.listen)?,
            threads: r.or("server.threads", d.threads)?,
            max_connections: r.or("server.max_connections", d.max_connections)?,
            max_body: bytes(r, "server.max_body", d.max_body)?,
            keep_alive: duration(r, "server.keep_alive", d.keep_alive)?,
            request_timeout: duration(r, "server.request_timeout", d.request_timeout)?,
            shutdown_grace: duration(r, "server.shutdown_grace", d.shutdown_grace)?,
        };
        if let Err(problem) = check_host_port(&config.listen) {
            return Err(r.invalid("server.listen", problem));
        }
        for (key, value) in [
            ("server.threads", config.threads),
            ("server.max_connections", config.max_connections),
        ] {
            if value == 0 {
                return Err(r.invalid(key, "must be at least 1"));
            }
        }
        if config.max_connections < config.threads {
            return Err(r.invalid(
                "server.max_connections",
                format!(
                    "must be at least server.threads ({}), got {}",
                    config.threads, config.max_connections
                ),
            ));
        }
        if config.max_body == 0 {
            return Err(r.invalid("server.max_body", "must be positive"));
        }
        Ok(config)
    }

    /// Whether `listen` only accepts connections from this machine.
    #[must_use]
    pub fn is_loopback(&self) -> bool {
        self.listen
            .parse::<SocketAddr>()
            .is_ok_and(|addr| addr.ip().is_loopback())
    }

    /// Whether `listen` names a wildcard address (`0.0.0.0`, `[::]`), which
    /// other nodes cannot use to reach this one.
    #[must_use]
    pub fn is_wildcard(&self) -> bool {
        self.listen
            .parse::<SocketAddr>()
            .is_ok_and(|addr| addr.ip().is_unspecified())
    }
}

/// Checks `host:port` syntax without resolving the host.
pub(crate) fn check_host_port(text: &str) -> std::result::Result<(), String> {
    if text.parse::<SocketAddr>().is_ok() {
        return Ok(());
    }
    let Some((host, port)) = text.rsplit_once(':') else {
        return Err(format!("expected host:port, got {text:?}"));
    };
    if port.parse::<u16>().is_err() {
        return Err(format!("invalid port {port:?} in {text:?}"));
    }
    let valid_host = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'));
    if valid_host {
        Ok(())
    } else {
        Err(format!("invalid host {host:?} in {text:?}"))
    }
}

/// `[storage]`: where and how jobs are kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageConfig {
    /// Data directory. Default `belay-data` (relative to the working
    /// directory).
    pub dir: PathBuf,
    /// Keep everything in memory and lose it on exit. Default false.
    pub in_memory: bool,
    /// When the write-ahead log is synced. Default `always`.
    pub sync: SyncPolicy,
    /// Log size that triggers a checkpoint; 0 disables size-triggered
    /// checkpoints. Default 128 MiB.
    pub checkpoint_bytes: u64,
    /// Longest time between checkpoints while anything is written; 0
    /// disables time-triggered checkpoints. Default 1h. Bounds recovery
    /// time on stores that grow too slowly to reach `checkpoint_bytes`.
    pub checkpoint_interval: Duration,
    /// Size at which a log segment is sealed. Default 32 MiB.
    pub segment_size: u64,
    /// How recovery treats damage a crash cannot explain. Default `strict`.
    pub recovery: RecoveryMode,
}

impl Default for StorageConfig {
    fn default() -> StorageConfig {
        StorageConfig {
            dir: PathBuf::from("belay-data"),
            in_memory: false,
            sync: SyncPolicy::Always,
            checkpoint_bytes: Options::DEFAULT_CHECKPOINT_BYTES,
            checkpoint_interval: Duration::from_secs(3600),
            segment_size: Options::DEFAULT_SEGMENT_SIZE,
            recovery: RecoveryMode::Strict,
        }
    }
}

impl StorageConfig {
    pub(crate) fn read(r: &Reader<'_>) -> Result<StorageConfig> {
        let d = StorageConfig::default();
        let dir: Option<String> = r.opt("storage.dir")?;
        let config = StorageConfig {
            dir: dir.map_or(d.dir, PathBuf::from),
            in_memory: r.or("storage.in_memory", d.in_memory)?,
            sync: r.opt_parsed("storage.sync")?.unwrap_or(d.sync),
            checkpoint_bytes: bytes(r, "storage.checkpoint_bytes", d.checkpoint_bytes)?,
            checkpoint_interval: duration(r, "storage.checkpoint_interval", d.checkpoint_interval)?,
            segment_size: bytes(r, "storage.segment_size", d.segment_size)?,
            recovery: r.opt_parsed("storage.recovery")?.unwrap_or(d.recovery),
        };
        if config.dir.as_os_str().is_empty() {
            return Err(r.invalid("storage.dir", "must not be empty"));
        }
        let range = Options::MIN_SEGMENT_SIZE..=Options::MAX_SEGMENT_SIZE;
        if !range.contains(&config.segment_size) {
            return Err(r.invalid(
                "storage.segment_size",
                format!(
                    "must be between {} and {} bytes, got {}",
                    range.start(),
                    range.end(),
                    config.segment_size
                ),
            ));
        }
        Ok(config)
    }

    /// Options for opening the log-structured store.
    #[must_use]
    pub fn store_options(&self) -> Options {
        let options = Options::new()
            .sync(self.sync)
            .segment_size(self.segment_size)
            .recovery(self.recovery);
        if self.checkpoint_bytes == 0 {
            options.manual_checkpoints()
        } else {
            options.checkpoint_every(self.checkpoint_bytes)
        }
    }
}

/// `[log]`: what is logged and how.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogConfig {
    /// Level directives, key `level`: `info`, `debug,belay_store=trace`.
    /// Default `info`.
    pub filter: Filter,
    /// Line format: `logfmt` (default) or `json`.
    pub format: LogFormat,
}

impl Default for LogConfig {
    fn default() -> LogConfig {
        LogConfig {
            filter: Filter::new(belay_common::log::Level::Info),
            format: LogFormat::Logfmt,
        }
    }
}

impl LogConfig {
    pub(crate) fn read(r: &Reader<'_>) -> Result<LogConfig> {
        let d = LogConfig::default();
        Ok(LogConfig {
            filter: r.opt_parsed("log.level")?.unwrap_or(d.filter),
            format: r.opt_parsed("log.format")?.unwrap_or(d.format),
        })
    }

    /// Installs a stderr logger with this filter and format as the
    /// process-wide logger.
    pub fn install(&self) {
        belay_common::log::init_stderr(self.filter.clone(), self.format);
    }
}

/// `[engine]`: queue engine settings that apply to every queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineConfig {
    /// Interval of the queue tick (promotions, lease expiry, retention).
    /// Default 250ms.
    pub tick: Duration,
    /// How a lease naming several queues picks between them:
    /// `round_robin` (default) or `in_order`.
    pub queue_order: QueueOrder,
    /// Silence after which a worker is reported lost. Default 2m.
    pub worker_timeout: Duration,
    /// How long a lost worker stays listed. Default 1h.
    pub worker_retention: Duration,
    /// Job changes per store batch in bulk operations. Default 256.
    pub batch_size: usize,
    /// Jobs a tenant takes per fair-share turn. Default 1.
    pub tenant_quantum: u32,
    /// Per-tenant multipliers of the quantum, `[engine.tenant_weights]`.
    pub tenant_weights: BTreeMap<TenantId, u32>,
}

impl Default for EngineConfig {
    fn default() -> EngineConfig {
        EngineConfig {
            tick: Duration::from_millis(250),
            queue_order: QueueOrder::RoundRobin,
            worker_timeout: EngineOptions::DEFAULT_WORKER_TIMEOUT,
            worker_retention: EngineOptions::DEFAULT_WORKER_RETENTION,
            batch_size: EngineOptions::DEFAULT_BATCH_SIZE,
            tenant_quantum: 1,
            tenant_weights: BTreeMap::new(),
        }
    }
}

impl EngineConfig {
    pub(crate) fn read(r: &Reader<'_>) -> Result<EngineConfig> {
        let d = EngineConfig::default();
        let raw_weights: BTreeMap<String, u32> = r.or("engine.tenant_weights", BTreeMap::new())?;
        let mut tenant_weights = BTreeMap::new();
        for (name, weight) in raw_weights {
            let path = belay_conf::KeyPath::root()
                .key("engine")
                .key("tenant_weights")
                .key(&name)
                .to_string();
            let tenant = TenantId::new(name).map_err(|e| r.invalid(&path, e.to_string()))?;
            if weight == 0 {
                return Err(r.invalid(&path, "must be at least 1"));
            }
            tenant_weights.insert(tenant, weight);
        }
        let config = EngineConfig {
            tick: duration(r, "engine.tick", d.tick)?,
            queue_order: r.opt_parsed("engine.queue_order")?.unwrap_or(d.queue_order),
            worker_timeout: duration(r, "engine.worker_timeout", d.worker_timeout)?,
            worker_retention: duration(r, "engine.worker_retention", d.worker_retention)?,
            batch_size: r.or("engine.batch_size", d.batch_size)?,
            tenant_quantum: r.or("engine.tenant_quantum", d.tenant_quantum)?,
            tenant_weights,
        };
        positive(r, "engine.tick", config.tick)?;
        positive(r, "engine.worker_timeout", config.worker_timeout)?;
        for (key, value) in [
            ("engine.batch_size", config.batch_size),
            ("engine.tenant_quantum", config.tenant_quantum as usize),
        ] {
            if value == 0 {
                return Err(r.invalid(key, "must be at least 1"));
            }
        }
        Ok(config)
    }

    /// Options for opening the queue engine.
    #[must_use]
    pub fn engine_options(&self) -> EngineOptions {
        let mut options = EngineOptions::new()
            .queue_order(self.queue_order)
            .worker_timeout(self.worker_timeout)
            .worker_retention(self.worker_retention)
            .batch_size(self.batch_size)
            .tenant_quantum(self.tenant_quantum);
        for (tenant, weight) in &self.tenant_weights {
            options = options.tenant_weight(tenant.clone(), *weight);
        }
        options
    }
}

/// `[scheduler]`: how cron schedules fire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchedulerConfig {
    /// Whether this node fires schedules at all. Default true.
    pub enabled: bool,
    /// Longest sleep of the scheduler loop; it normally wakes exactly at
    /// the next fire time, and this bounds how late it notices schedules
    /// changed by other means. Default 1s.
    pub tick: Duration,
    /// How late a fire may be and still count as on time. Default 60s.
    pub misfire_threshold: Duration,
    /// How long a fire's deduplication key is held. Default 24h.
    pub dedupe_window: Duration,
    /// Wait before retrying a fire the queue could not take. Default 5s.
    pub retry_delay: Duration,
}

impl Default for SchedulerConfig {
    fn default() -> SchedulerConfig {
        let o = SchedulerOptions::new();
        SchedulerConfig {
            enabled: true,
            tick: Duration::from_secs(1),
            misfire_threshold: o.get_misfire_threshold(),
            dedupe_window: o.get_dedupe_window(),
            retry_delay: o.get_retry_delay(),
        }
    }
}

impl SchedulerConfig {
    pub(crate) fn read(r: &Reader<'_>) -> Result<SchedulerConfig> {
        let d = SchedulerConfig::default();
        let config = SchedulerConfig {
            enabled: r.or("scheduler.enabled", d.enabled)?,
            tick: duration(r, "scheduler.tick", d.tick)?,
            misfire_threshold: duration(r, "scheduler.misfire_threshold", d.misfire_threshold)?,
            dedupe_window: duration(r, "scheduler.dedupe_window", d.dedupe_window)?,
            retry_delay: duration(r, "scheduler.retry_delay", d.retry_delay)?,
        };
        positive(r, "scheduler.tick", config.tick)?;
        positive(r, "scheduler.retry_delay", config.retry_delay)?;
        if let Err(e) = config.scheduler_options().validate() {
            let message = e.to_string();
            return Err(r.invalid(
                "scheduler.dedupe_window",
                message.trim_start_matches("invalid options: "),
            ));
        }
        Ok(config)
    }

    /// Options for opening the scheduler.
    #[must_use]
    pub fn scheduler_options(&self) -> SchedulerOptions {
        SchedulerOptions::new()
            .misfire_threshold(self.misfire_threshold)
            .dedupe_window(self.dedupe_window)
            .retry_delay(self.retry_delay)
    }
}

/// `[workflows]`: workflow bookkeeping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowsConfig {
    /// How long finished runs are kept. Default 7d.
    pub keep_for: Duration,
    /// Interval of the workflow chores: deleting runs past `keep_for` and
    /// reconciling running steps with their jobs. Default 30s.
    pub sweep_every: Duration,
    /// How long a step's deduplication key is held. Default 24h.
    pub dedupe_window: Duration,
}

impl Default for WorkflowsConfig {
    fn default() -> WorkflowsConfig {
        WorkflowsConfig {
            keep_for: Duration::from_secs(7 * 86_400),
            sweep_every: Duration::from_secs(30),
            dedupe_window: WorkflowOptions::new().get_dedupe_window(),
        }
    }
}

impl WorkflowsConfig {
    pub(crate) fn read(r: &Reader<'_>) -> Result<WorkflowsConfig> {
        let d = WorkflowsConfig::default();
        let config = WorkflowsConfig {
            keep_for: duration(r, "workflows.keep_for", d.keep_for)?,
            sweep_every: duration(r, "workflows.sweep_every", d.sweep_every)?,
            dedupe_window: duration(r, "workflows.dedupe_window", d.dedupe_window)?,
        };
        positive(r, "workflows.sweep_every", config.sweep_every)?;
        if let Err(e) = config.workflow_options().validate() {
            let message = e.to_string();
            return Err(r.invalid(
                "workflows.dedupe_window",
                message.trim_start_matches("invalid options: "),
            ));
        }
        Ok(config)
    }

    /// Options for opening the workflow engine.
    #[must_use]
    pub fn workflow_options(&self) -> WorkflowOptions {
        WorkflowOptions::new().dedupe_window(self.dedupe_window)
    }
}

/// `[events]`: the in-memory event stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventsConfig {
    /// Events kept for replay to subscribers that resume. Default 10000.
    pub buffer: usize,
    /// Events queued per live subscriber before it counts as lagging and
    /// is switched to reading from the replay buffer. Default 1024.
    pub subscriber_queue: usize,
}

impl Default for EventsConfig {
    fn default() -> EventsConfig {
        EventsConfig {
            buffer: 10_000,
            subscriber_queue: 1024,
        }
    }
}

impl EventsConfig {
    pub(crate) fn read(r: &Reader<'_>) -> Result<EventsConfig> {
        let d = EventsConfig::default();
        let config = EventsConfig {
            buffer: r.or("events.buffer", d.buffer)?,
            subscriber_queue: r.or("events.subscriber_queue", d.subscriber_queue)?,
        };
        for (key, value) in [
            ("events.buffer", config.buffer),
            ("events.subscriber_queue", config.subscriber_queue),
        ] {
            if value == 0 {
                return Err(r.invalid(key, "must be at least 1"));
            }
        }
        Ok(config)
    }
}

/// Timing presets of the membership protocol.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClusterProfile {
    /// One data centre or cloud region: 1s probes. The default.
    #[default]
    Lan,
    /// Members spread across regions: slower probes, longer timeouts.
    Wan,
    /// Processes on one machine (development, tests): sub-second failure
    /// detection.
    Local,
}

impl fmt::Display for ClusterProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ClusterProfile::Lan => "lan",
            ClusterProfile::Wan => "wan",
            ClusterProfile::Local => "local",
        })
    }
}

impl FromStr for ClusterProfile {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<ClusterProfile, String> {
        match s {
            "lan" => Ok(ClusterProfile::Lan),
            "wan" => Ok(ClusterProfile::Wan),
            "local" => Ok(ClusterProfile::Local),
            _ => Err(format!(
                "unknown cluster profile {s:?}, expected lan, wan or local"
            )),
        }
    }
}
