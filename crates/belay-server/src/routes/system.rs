//! Probes, metrics and identity: `/healthz`, `/readyz`, `/metrics`,
//! `/v1/info`.
//!
//! `/healthz` answers 200 as long as the process can answer at all; it is
//! what a supervisor should restart on. `/readyz` answers 503 while the
//! node should not receive traffic: while it joins its cluster, and from
//! the moment a shutdown begins (server drain or engine stop), so load
//! balancers stop routing to it before its connections close.
//!
//! Probes and scrapes ignore query parameters: monitoring systems like to
//! add their own, and a probe that fails over one would be worse than
//! useless.

use belay_http::{Request, Response};
use belay_proto::api::ApiError;

use crate::app::{server_draining, App};
use crate::extract::only_params;
use crate::reply;

pub(super) fn healthz(_: &App, _: &mut Request) -> Result<Response, ApiError> {
    Ok(Response::text("ok\n"))
}

pub(super) fn readyz(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    let engine = &app.engine;
    if server_draining(req) || engine.is_shutting_down() {
        return Err(ApiError::unavailable("shutting down"));
    }
    if !engine.is_ready() {
        return Err(ApiError::unavailable(
            "not ready: waiting to join the cluster",
        ));
    }
    Ok(Response::text("ready\n"))
}

pub(super) fn metrics(app: &App, _: &mut Request) -> Result<Response, ApiError> {
    Ok(Response::bytes(
        belay_metrics::text::CONTENT_TYPE,
        app.engine.render_metrics(),
    ))
}

pub(super) fn info(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    only_params(req, &[])?;
    Ok(reply::ok(&app.engine.info()))
}

#[cfg(test)]
mod tests {
    use belay_http::{Handler, Method, ShutdownSignal};

    use crate::routes::testing;

    #[test]
    fn readiness_turns_off_as_soon_as_the_server_drains() {
        let engine = testing::engine();
        let router = testing::router(&engine);
        let signal = ShutdownSignal::new();
        let probe = || {
            let mut req = testing::request(Method::Get, "/readyz", "");
            req.extensions.insert(signal.clone());
            testing::call(&router, req)
        };
        assert_eq!(probe().0, 200);
        signal.trigger();
        let (status, body) = probe();
        assert_eq!(status, 503);
        assert_eq!(body["error"]["message"], "shutting down");
        // The engine itself is still running: only the server is leaving.
        assert!(!engine.is_shutting_down());
    }

    #[test]
    fn probes_ignore_query_parameters_but_the_api_does_not() {
        let engine = testing::engine();
        let router = testing::router(&engine);
        for target in ["/healthz?probe=lb", "/readyz?x=1", "/metrics?format=text"] {
            let (status, _) = testing::call(&router, testing::request(Method::Get, target, ""));
            assert_eq!(status, 200, "{target}");
        }
        let (status, _) = testing::call(&router, testing::request(Method::Get, "/v1/info?x=1", ""));
        assert_eq!(status, 400);
    }

    #[test]
    fn metrics_use_the_prometheus_content_type() {
        let engine = testing::engine();
        let router = testing::router(&engine);
        let mut req = testing::request(Method::Get, "/metrics", "");
        let response = router.call(&mut req);
        assert_eq!(
            response.headers.get("content-type"),
            Some(belay_metrics::text::CONTENT_TYPE)
        );
        let text = String::from_utf8_lossy(response.body_bytes().expect("in memory"));
        assert!(text.contains("# TYPE belay_info gauge"), "{text}");
    }
}
