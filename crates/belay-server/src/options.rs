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
