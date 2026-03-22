//! `/v1/schedules`: cron schedules.
//!
//! `POST` creates (`409 already_exists` if the id is taken); `PUT` creates
//! or replaces (`201` when it created, `200` otherwise) and may leave the
//! `id` out of the body, since the path names it. Every call about one
//! schedule is served by the node that fires it; the listing shows the
//! schedules this node stores.

use belay_http::{Request, Response};
use belay_proto::api::ApiError;
use belay_proto::{ScheduleId, ScheduleSpec};

use belay_core::Upserted;

use crate::app::App;
use crate::extract::{self, only_params};
use crate::placement;
use crate::reply;

/// Fire times returned by a preview without `?n=`.
const DEFAULT_PREVIEW: usize = 10;

/// Most fire times one preview returns.
const MAX_PREVIEW: usize = 1000;

fn schedule_path(id: &ScheduleId) -> String {
    format!("/v1/schedules/{id}")
}

pub(super) fn list(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    let paging = extract::paging(req, &[])?;
    Ok(reply::ok(&app.engine.list_schedules(&paging)?))
}

pub(super) fn create(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    only_params(req, &[])?;
    let spec: ScheduleSpec = extract::body(req)?;
    if let Some(redirect) = placement::for_schedule(&app.engine, req, &spec.id) {
        return Ok(redirect);
    }
    let schedule = app.engine.create_schedule(spec)?;
    Ok(reply::created(&schedule, &schedule_path(&schedule.spec.id)))
}

/// Runs `op` on the schedule named in the path, unless another node owns
/// it.
fn on_schedule(
    app: &App,
    req: &mut Request,
    params: &[&str],
    op: impl FnOnce(&App, &Request, &ScheduleId) -> Result<Response, ApiError>,
) -> Result<Response, ApiError> {
    only_params(req, params)?;
    let id = extract::schedule_id(req)?;
    if let Some(redirect) = placement::for_schedule(&app.engine, req, &id) {
        return Ok(redirect);
    }
    op(app, req, &id)
}

pub(super) fn get(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    on_schedule(app, req, &[], |app, _, id| {
        Ok(reply::ok(&app.engine.get_schedule(id)?))
    })
}

pub(super) fn put(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    on_schedule(app, req, &[], |app, req, id| {
        let spec: ScheduleSpec = extract::body_named(req, "id", id.as_str())?;
        let (schedule, outcome) = app.engine.put_schedule(spec)?;
        Ok(match outcome {
            Upserted::Created => reply::created(&schedule, &schedule_path(id)),
            Upserted::Updated | Upserted::Unchanged => reply::ok(&schedule),
        })
    })
}

/// Answers with the schedule's last state; jobs it created are kept.
pub(super) fn delete(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    on_schedule(app, req, &[], |app, _, id| {
        Ok(reply::ok(&app.engine.delete_schedule(id)?))
    })
}
