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
