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
