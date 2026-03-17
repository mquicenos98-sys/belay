//! `/v1/queues`: status, configuration, pausing, purging and deletion.
//!
//! Queues come into existence when a job is enqueued into them or when
//! they are configured, paused or resumed; there is no separate "create".
//! Every call naming a queue is redirected to its owner in a cluster; the
//! listing shows the queues this node knows.

use belay_http::{Request, Response};
use belay_proto::api::ApiError;
use belay_proto::{JobState, QueueConfig};

use crate::app::App;
use crate::extract::{self, only_params};
use crate::placement;
use crate::reply;

pub(super) fn list(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    let paging = extract::paging(req, &[])?;
    Ok(reply::ok(&app.engine.list_queues(&paging)?))
}

/// Runs `op` on the queue named in the path, unless another node owns it.
fn on_queue(
    app: &App,
    req: &mut Request,
    params: &[&str],
    op: impl FnOnce(&App, &Request, &belay_proto::QueueName) -> Result<Response, ApiError>,
) -> Result<Response, ApiError> {
    only_params(req, params)?;
    let name = extract::queue_name(req)?;
    if let Some(redirect) = placement::for_queue(&app.engine, req, &name) {
        return Ok(redirect);
    }
    op(app, req, &name)
}

pub(super) fn get(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    on_queue(app, req, &[], |app, _, name| {
        Ok(reply::ok(&app.engine.queue_info(name)?))
    })
}
