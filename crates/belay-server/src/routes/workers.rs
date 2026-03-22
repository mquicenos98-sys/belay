//! `GET /v1/workers`: the workers this node has heard from, built from
//! their lease and heartbeat traffic. A worker counts as lost after
//! `engine.worker_timeout` of silence and is listed for
//! `engine.worker_retention` more; a worker polling several nodes of a
//! cluster shows up on each of them.

use belay_http::{Request, Response};
use belay_proto::api::ApiError;

use crate::app::App;
use crate::extract;
use crate::reply;

pub(super) fn list(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    let paging = extract::paging(req, &[])?;
    Ok(reply::ok(&app.engine.workers(&paging)?))
}
