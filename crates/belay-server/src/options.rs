//! What the server takes besides the engine's `[server]` configuration:
//! per-token request rates and the pacing of event streams and long polls
//! ([`ServerOptions`]), plus the translation of `[server]` into the HTTP
//! layer's settings.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use belay_common::{error, warn};
use belay_core::config::ServerConfig as ApiServerConfig;
use belay_http::{ServerConfig as HttpConfig, ServerError};

/// A token bucket: `burst` requests at once, refilled at `per_second`.
///
/// ```
/// use belay_server::RequestRate;
///
/// let rate = RequestRate::new(50.0, 100);
/// assert_eq!(rate.burst, 100);
/// assert!(RequestRate::DEFAULT.per_second >= 1000.0);
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RequestRate {
    /// Sustained requests per second.
    pub per_second: f64,
    /// Requests allowed in a burst, and the bucket's capacity.
    pub burst: u32,
}

impl RequestRate {
    /// 1000 requests per second with bursts of 2000: a guard against a
    /// runaway client, not a quota. One token is often shared by a whole
    /// worker fleet, and every job costs a fleet at least two requests
    /// (lease and complete) plus its heartbeats.
    pub const DEFAULT: RequestRate = RequestRate {
        per_second: 1000.0,
        burst: 2000,
    };

    /// A rate of `per_second` with bursts of `burst` (at least 1).
    ///
    /// # Panics
    ///
    /// If `per_second` is not a positive, finite number: a limit that can
    /// never refill is a programming error, not something to discover
    /// under load.
    #[must_use]
    pub fn new(per_second: f64, burst: u32) -> RequestRate {
        assert!(
            per_second.is_finite() && per_second > 0.0,
            "a request rate must be positive and finite, got {per_second}"
        );
        RequestRate {
            per_second,
            burst: burst.max(1),
        }
    }
}

/// Server settings that are not part of the engine configuration.
///
/// Construct with struct-update syntax or the `with_` helpers:
///
/// ```
/// use std::time::Duration;
/// use belay_server::{RequestRate, ServerOptions};
///
/// let options = ServerOptions {
///     event_heartbeat: Duration::from_secs(5),
///     ..ServerOptions::default()
/// }
/// .with_rate_limit(Some(RequestRate::new(200.0, 400)))
/// .with_token_rate_limit("ops", None);
/// assert_eq!(options.rate_for("ci"), Some(RequestRate::new(200.0, 400)));
/// assert_eq!(options.rate_for("ops"), None);
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct ServerOptions {
    /// Requests per second allowed to each token; `None` for no limit.
    /// Default [`RequestRate::DEFAULT`]. Only applies while authentication
    /// is enforced: without tokens every caller is the same anonymous
    /// principal, and one shared bucket would throttle everybody at once.
    pub rate_limit: Option<RequestRate>,
    /// Overrides of `rate_limit` by token name; `None` exempts a token.
    pub token_rate_limits: BTreeMap<String, Option<RequestRate>>,
    /// An event stream with nothing to send writes a comment this often,
    /// which keeps proxies from closing it and reveals clients that went
    /// away. Default 15 s.
    pub event_heartbeat: Duration,
    /// Sent as `retry:` at the start of every event stream: how long a
    /// client waits before reconnecting. Default 2 s.
    pub event_retry: Duration,
    /// How often a long-polling lease and an event stream check whether
    /// the server is shutting down (and a stream whether its client left).
    /// Bounds how long those requests delay a graceful shutdown. Default
    /// 250 ms.
    pub shutdown_poll: Duration,
}

impl Default for ServerOptions {
    fn default() -> ServerOptions {
        ServerOptions {
            rate_limit: Some(RequestRate::DEFAULT),
            token_rate_limits: BTreeMap::new(),
            event_heartbeat: belay_http::DEFAULT_HEARTBEAT,
            event_retry: Duration::from_secs(2),
            shutdown_poll: Duration::from_millis(250),
        }
    }
}

impl ServerOptions {
    /// The defaults.
    #[must_use]
    pub fn new() -> ServerOptions {
        ServerOptions::default()
    }

    /// Sets [`rate_limit`](Self::rate_limit).
    #[must_use]
    pub fn with_rate_limit(mut self, rate: Option<RequestRate>) -> ServerOptions {
        self.rate_limit = rate;
        self
    }

    /// Gives the token named `token` its own rate (`None`: unlimited).
    #[must_use]
    pub fn with_token_rate_limit(
        mut self,
        token: impl Into<String>,
        rate: Option<RequestRate>,
    ) -> ServerOptions {
        self.token_rate_limits.insert(token.into(), rate);
        self
    }

    /// The rate that applies to the token named `token`.
    #[must_use]
    pub fn rate_for(&self, token: &str) -> Option<RequestRate> {
        match self.token_rate_limits.get(token) {
            Some(rate) => *rate,
            None => self.rate_limit,
        }
    }

    /// The poll interval, never zero (a zero interval would spin).
    pub(crate) fn poll(&self) -> Duration {
        self.shutdown_poll.max(Duration::from_millis(1))
    }
}

/// The largest body the HTTP layer reads at all for a configured
/// `max_body`: twice the limit, and at least 64 KiB over it.
///
/// Bodies above `max_body` but within this are read and refused by the
/// server's own middleware with a JSON `413`, which clients can decode.
/// The HTTP layer refuses anything larger before reading it, with a
/// plain-text `413`: reading unbounded bodies just to phrase the refusal
/// nicely would hand every client a way to make the server buffer
/// gigabytes.
#[must_use]
pub(crate) fn read_limit(max_body: u64) -> usize {
    let limit = max_body
        .saturating_mul(2)
        .max(max_body.saturating_add(64 * 1024));
    usize::try_from(limit).unwrap_or(usize::MAX)
}

/// The HTTP layer's settings for the engine's `[server]` section.
///
/// `server.threads` has no counterpart: belay-http serves each connection
/// on its own thread from a pool that grows up to `max_connections` and
/// shrinks when idle, which is what lets long polls and event streams
/// block without starving anybody.
pub(crate) fn http_config(server: &ApiServerConfig) -> HttpConfig {
    HttpConfig {
        max_connections: server.max_connections,
        keep_alive_timeout: server.keep_alive,
        header_timeout: server.request_timeout,
        body_timeout: server.request_timeout,
        max_body_bytes: read_limit(server.max_body),
        shutdown_grace: server.shutdown_grace,
        server_header: Some(format!("belay/{}", crate::VERSION)),
        error_hook: Some(Arc::new(log_server_error)),
        ..HttpConfig::default()
    }
}
