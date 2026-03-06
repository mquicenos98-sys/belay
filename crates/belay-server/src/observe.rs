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

/// Methods outside the standard set share one label value: a method token
/// is chosen by the client, and each distinct one would be a new series.
fn method_label(method: &Method) -> &'static str {
    match method {
        Method::Get => "GET",
        Method::Head => "HEAD",
        Method::Post => "POST",
        Method::Put => "PUT",
        Method::Delete => "DELETE",
        Method::Patch => "PATCH",
        Method::Options => "OPTIONS",
        _ => "other",
    }
}

/// The access log and metrics middleware.
#[derive(Debug, Clone)]
pub(crate) struct Observe {
    metrics: Option<HttpMetrics>,
}

impl Observe {
    pub(crate) fn new(metrics: Option<HttpMetrics>) -> Observe {
        Observe { metrics }
    }
}

impl Middleware for Observe {
    fn handle(&self, req: &mut Request, next: Next<'_>) -> Response {
        let started = Instant::now();
        let in_flight = self.metrics.as_ref().map(|m| m.in_flight.track_in_flight());
        let response = next.run(req);
        let elapsed = started.elapsed();
        drop(in_flight);
        let route = req
            .extensions
            .get::<MatchedRoute>()
            .map(MatchedRoute::as_str);
        if let Some(metrics) = &self.metrics {
            metrics.record(
                route.unwrap_or(UNMATCHED),
                &req.method,
                response.status,
                elapsed,
            );
        }
        log_access(req, &response, route, elapsed);
        response
    }
}

/// Probes stay at `debug` even when they fail: a draining node answers
/// `/readyz` with 503 on purpose, every few seconds.
fn access_level(route: Option<&str>, status: StatusCode) -> Level {
    if route.is_some_and(|r| PROBES.contains(&r)) {
        Level::Debug
    } else if status.is_server_error() {
        Level::Warn
    } else {
        Level::Info
    }
}

fn log_access(req: &Request, response: &Response, route: Option<&str>, elapsed: Duration) {
    let status = response.status.as_u16();
    let principal = req.extensions.get::<Principal>().map(|p| p.name.as_str());
    let request_id = req.extensions.get::<RequestId>().map(RequestId::as_str);
    belay_common::log!(
        access_level(route, response.status),
        target: ACCESS_LOG_TARGET,
        "{} {} {}", req.method, req.path, status;
        route = route.unwrap_or("-"),
        status,
        duration = elapsed,
        principal = principal.unwrap_or("-"),
        request_id = request_id.unwrap_or("-"),
        remote = req.remote_addr.map(|a| a.to_string()),
        bytes_in = req.body.len(),
        bytes_out = response.body.known_len(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_common::log::CaptureLogger;
    use belay_core::config::Scopes;
    use belay_http::middleware::SetRequestId;
    use belay_http::{Handler, Router};

    fn router(metrics: Option<HttpMetrics>) -> Router {
        let mut router = Router::new();
        router
            .wrap(SetRequestId::new())
            .wrap(Observe::new(metrics))
            .get("/v1/jobs/:id", |req: &mut Request| {
                req.extensions.insert(Principal {
                    name: "ci".to_owned(),
                    scopes: Scopes::all(),
                });
                Response::text("job")
            })
            .get("/healthz", |_: &mut Request| Response::text("ok"))
            .get("/boom", |_: &mut Request| {
                Response::new(StatusCode::INTERNAL_SERVER_ERROR)
            });
        router
    }

    fn call(router: &Router, method: Method, target: &str) -> Response {
        let mut req = Request::new(method, target).with_header("X-Request-Id", "req-7");
        router.call(&mut req)
    }

    #[test]
    fn access_lines_carry_route_principal_and_request_id() {
        let capture = CaptureLogger::new();
        let router = router(None);
        capture.scope(|| call(&router, Method::Get, "/v1/jobs/01H"));
        let records = capture.records();
        assert_eq!(records.len(), 1);
        let line = &records[0];
        assert_eq!(line.target, ACCESS_LOG_TARGET);
        assert_eq!(line.level, Level::Info);
        assert_eq!(line.message, "GET /v1/jobs/01H 200");
        assert_eq!(line.field("route"), Some("/v1/jobs/:id"));
        assert_eq!(line.field("principal"), Some("ci"));
        assert_eq!(line.field("request_id"), Some("req-7"));
        assert_eq!(line.field("bytes_out"), Some("3"));
    }

    #[test]
    fn probes_log_at_debug_and_server_errors_at_warn() {
        let capture = CaptureLogger::new();
        let router = router(None);
        capture.scope(|| {
            call(&router, Method::Get, "/healthz");
            call(&router, Method::Get, "/boom");
            call(&router, Method::Get, "/nowhere");
        });
        let levels: Vec<(Level, Option<String>)> = capture
            .records()
            .iter()
            .map(|r| (r.level, r.field("route").map(str::to_owned)))
            .collect();
        assert_eq!(
            levels,
            [
                (Level::Debug, Some("/healthz".to_owned())),
                (Level::Warn, Some("/boom".to_owned())),
                (Level::Info, Some("-".to_owned())),
            ]
        );
    }

    #[test]
    fn metrics_use_route_patterns_and_bounded_labels() {
        let registry = Registry::new();
        let metrics = HttpMetrics::register(&registry).unwrap();
        let router = router(Some(metrics.clone()));
        call(&router, Method::Get, "/v1/jobs/a");
        call(&router, Method::Get, "/v1/jobs/b");
        call(&router, Method::Get, "/random/path");
        call(
            &router,
            Method::parse("PURGE").expect("valid token"),
            "/v1/jobs/a",
        );
        let text = registry.render_prometheus();
        assert!(
            text.contains(
                "belay_http_requests_total{route=\"/v1/jobs/:id\",method=\"GET\",status=\"200\"} 2\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "belay_http_requests_total{route=\"unmatched\",method=\"GET\",status=\"404\"} 1\n"
            ),
            "{text}"
        );
        // A method the route does not take matches no route either.
        assert!(
            text.contains(
                "belay_http_requests_total{route=\"unmatched\",method=\"other\",status=\"405\"} 1\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("belay_http_request_duration_seconds_count{route=\"/v1/jobs/:id\"} 2\n"),
            "{text}"
        );
        assert!(text.contains("belay_http_requests_in_flight 0\n"), "{text}");
        // A second registration hands back the same families.
        let again = HttpMetrics::register(&registry).unwrap();
        again.rate_limited("ci");
        assert!(registry
            .render_prometheus()
            .contains("belay_http_rate_limited_total{token=\"ci\"} 1\n"));
    }

    #[test]
    fn open_streams_are_counted_until_their_guard_drops() {
        let registry = Registry::new();
        let metrics = HttpMetrics::register(&registry).unwrap();
        let guard = metrics.stream_opened();
        assert_eq!(metrics.streams.get(), 1.0);
        drop(guard);
        assert_eq!(metrics.streams.get(), 0.0);
    }
}
