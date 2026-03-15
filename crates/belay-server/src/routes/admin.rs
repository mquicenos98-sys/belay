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
