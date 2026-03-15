//! `/v1/admin/*`: store checkpoints and statistics, the consistency check,
//! and a graceful shutdown over HTTP.
//!
//! The standard library has no signal handling, so `POST
//! /v1/admin/shutdown` is how an operator (or a supervisor script) stops
//! a server cleanly: it triggers the same drain as
//! [`ServerHandle::shutdown`](crate::ServerHandle::shutdown). The request
//! itself is answered (`202 Accepted`) before the connection closes.
//! Killing the process instead is safe too: the store is crash-consistent,
//! so nothing acknowledged is lost, but running jobs only return to their
//! queues once their leases expire.

use belay_common::{format_duration, warn};
use belay_core::config::Principal;
use belay_http::{Request, Response, ShutdownSignal, StatusCode};
use belay_json::json;
use belay_proto::api::ApiError;

use crate::app::App;
use crate::extract::only_params;
use crate::reply;

pub(super) fn checkpoint(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    only_params(req, &[])?;
    Ok(reply::ok(&app.engine.checkpoint()?))
}

pub(super) fn store(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    only_params(req, &[])?;
    Ok(reply::ok(&app.engine.store_report()))
}

/// Rebuilds the queue's indexes from the store and compares them with the
/// live ones. Holds the queue lock for a full scan, which is why it needs
/// `admin` although it changes nothing.
pub(super) fn verify(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    only_params(req, &[])?;
    Ok(reply::ok(&app.engine.verify()?))
}

pub(super) fn shutdown(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    only_params(req, &[])?;
    let Some(signal) = req.extensions.get::<ShutdownSignal>() else {
        return Err(ApiError::failed_precondition(
            "this router is not running inside a server that can be shut down",
        ));
    };
    let already = signal.is_triggered();
    let by = req
        .extensions
        .get::<Principal>()
        .map_or("-", |p| p.name.as_str());
    if !already {
        warn!("shutdown requested over HTTP"; by);
        signal.trigger();
    }
    let grace = app.engine.config().server.shutdown_grace;
    let body = json!({
        "shutting_down": true,
        "already": already,
        "grace": format_duration(grace),
    });
    Ok(reply::json(StatusCode::ACCEPTED, &body))
}

#[cfg(test)]
mod tests {
    use belay_http::{Method, ShutdownSignal};

    use crate::routes::testing;

    #[test]
    fn shutdown_triggers_the_server_signal_once() {
        let engine = testing::engine();
        let router = testing::router(&engine);
        let signal = ShutdownSignal::new();
        let stop = || {
            let mut req = testing::request(Method::Post, "/v1/admin/shutdown", "");
            req.extensions.insert(signal.clone());
            testing::call(&router, req)
        };
        let (status, body) = stop();
        assert_eq!(status, 202);
        assert_eq!(body["already"], false);
        assert_eq!(body["grace"], "30s");
        assert!(signal.is_triggered());
        let (status, body) = stop();
        assert_eq!(status, 202);
        assert_eq!(body["already"], true);
        // The engine is stopped by whoever runs the server, after the drain.
        assert!(!engine.is_shutting_down());
    }

    #[test]
    fn checkpoints_store_reports_and_verification_answer_json() {
        let engine = testing::engine();
        let router = testing::router(&engine);
        let (status, body) = testing::call(
            &router,
            testing::request(Method::Post, "/v1/admin/checkpoint", ""),
        );
        assert_eq!(status, 200);
        assert!(body["seq"].as_u64().is_some(), "{body}");
        let (_, store) = testing::call(
            &router,
            testing::request(Method::Get, "/v1/admin/store", ""),
        );
        assert_eq!(store["storage"], "memory");
        let (_, verify) = testing::call(
            &router,
            testing::request(Method::Get, "/v1/admin/verify", ""),
        );
        assert_eq!(verify["consistent"], true);
    }
}
