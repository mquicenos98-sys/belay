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
