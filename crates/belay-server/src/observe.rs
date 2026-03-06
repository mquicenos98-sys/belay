//! The access log and the server's own metrics, recorded by one
//! middleware around every request.
//!
//! Each request produces one log line under the target
//! [`ACCESS_LOG_TARGET`]: `info` normally, `warn` for server errors, and
//! `debug` for the probes (`/healthz`, `/readyz`, `/metrics`) that load
//! balancers and scrapers call every few seconds. The line carries the
//! route pattern, status, duration, the principal (token name) and the
//! request id that is also returned in `X-Request-Id`.
//!
//! | family | type | labels |
//! |---|---|---|
//! | `belay_http_requests_total` | counter | `route`, `method`, `status` |
//! | `belay_http_request_duration_seconds` | histogram | `route` |
//! | `belay_http_requests_in_flight` | gauge | |
//! | `belay_http_event_streams` | gauge | |
//! | `belay_http_rate_limited_total` | counter | `token` |
//!
//! Labels use the route *pattern* (`/v1/jobs/:id`), never the path, and
//! requests that matched no route share `route="unmatched"`, so a client
//! probing random URLs cannot blow up the number of series. Durations end
//! when the handler returns: for an event stream that is when streaming
//! starts, for a long-polling lease when it is answered.

use std::fmt;
use std::time::{Duration, Instant};

use belay_common::log::Level;
use belay_core::config::Principal;
use belay_http::middleware::RequestId;
use belay_http::{MatchedRoute, Method, Middleware, Next, Request, Response, StatusCode};
use belay_metrics::{CounterVec, Gauge, HistogramVec, InFlight, Registry};

/// The log target of access log lines.
pub const ACCESS_LOG_TARGET: &str = "belay_server::access";

/// The `route` label of requests no route matched.
const UNMATCHED: &str = "unmatched";

/// Request duration buckets, in seconds: sub-millisecond reads up to the
/// longest long-poll wait (60 s).
const DURATION_BUCKETS: [f64; 14] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
];

/// Routes whose requests are logged at `debug`.
const PROBES: [&str; 3] = ["/healthz", "/readyz", "/metrics"];

/// Handles on the server's metric families.
#[derive(Clone)]
pub(crate) struct HttpMetrics {
    requests: CounterVec,
    duration: HistogramVec,
    in_flight: Gauge,
    streams: Gauge,
    rate_limited: CounterVec,
}

impl fmt::Debug for HttpMetrics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpMetrics")
            .field("in_flight", &self.in_flight.get())
            .field("streams", &self.streams.get())
            .finish_non_exhaustive()
    }
}

impl HttpMetrics {
    /// Registers the families in `registry`. Registering again (a second
    /// router over the same engine) returns the same families.
    pub(crate) fn register(registry: &Registry) -> belay_metrics::Result<HttpMetrics> {
        Ok(HttpMetrics {
            requests: registry.counter_vec(
                "belay_http_requests_total",
                "HTTP requests answered, by route pattern, method and status.",
                &["route", "method", "status"],
            )?,
            duration: registry.histogram_vec(
                "belay_http_request_duration_seconds",
                "Time to produce a response (for streams: until streaming starts).",
                &["route"],
                &DURATION_BUCKETS,
            )?,
            in_flight: registry.gauge(
                "belay_http_requests_in_flight",
                "Requests being handled, long polls included.",
            )?,
            streams: registry.gauge(
                "belay_http_event_streams",
                "Open event streams (GET /v1/events).",
            )?,
            rate_limited: registry.counter_vec(
                "belay_http_rate_limited_total",
                "Requests refused because their token was over its rate.",
                &["token"],
            )?,
        })
    }

    fn record(&self, route: &str, method: &Method, status: StatusCode, elapsed: Duration) {
        let status = status.as_u16().to_string();
        self.requests
            .with_label_values(&[route, method_label(method), &status])
            .inc();
        self.duration
            .with_label_values(&[route])
            .observe_duration(elapsed);
    }

    /// Counts a request refused for its token's rate.
    pub(crate) fn rate_limited(&self, token: &str) {
        self.rate_limited.with_label_values(&[token]).inc();
    }

    /// Counts an event stream as open until the guard is dropped.
    pub(crate) fn stream_opened(&self) -> InFlight {
        self.streams.track_in_flight()
    }
}
