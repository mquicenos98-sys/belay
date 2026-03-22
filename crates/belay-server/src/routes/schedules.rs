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

pub(super) fn pause(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    on_schedule(app, req, &[], |app, _, id| {
        Ok(reply::ok(&app.engine.pause_schedule(id)?))
    })
}

pub(super) fn resume(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    on_schedule(app, req, &[], |app, _, id| {
        Ok(reply::ok(&app.engine.resume_schedule(id)?))
    })
}

pub(super) fn trigger(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    on_schedule(app, req, &[], |app, _, id| {
        Ok(reply::ok(&app.engine.trigger_schedule(id)?))
    })
}

/// `?n=` fire times (default 10, at most 1000), before jitter and calendar
/// exclusions.
pub(super) fn preview(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    on_schedule(app, req, &["n"], |app, req, id| {
        let n = extract::query_value::<usize>(req, "n")?.unwrap_or(DEFAULT_PREVIEW);
        if !(1..=MAX_PREVIEW).contains(&n) {
            return Err(ApiError::invalid_argument(format!(
                "$.n: must be between 1 and {MAX_PREVIEW}, got {n}"
            )));
        }
        Ok(reply::ok(&app.engine.preview_schedule(id, n)?))
    })
}

#[cfg(test)]
mod tests {
    use belay_http::Method;

    use crate::routes::testing;

    const SPEC: &str = r#"{"cron": "*/5 * * * *", "template": {"queue": "q", "kind": "k"}}"#;

    #[test]
    fn previews_default_to_ten_fire_times() {
        let engine = testing::engine();
        let router = testing::router(&engine);
        let put = testing::request(Method::Put, "/v1/schedules/tick", SPEC);
        assert_eq!(testing::call(&router, put).0, 201);
        let preview = testing::request(Method::Get, "/v1/schedules/tick/preview", "");
        let (_, body) = testing::call(&router, preview);
        assert_eq!(body["times"].as_array().map(Vec::len), Some(10));
        let zero = testing::request(Method::Get, "/v1/schedules/tick/preview?n=0", "");
        assert_eq!(testing::call(&router, zero).0, 400);
    }

    #[test]
    fn put_reports_whether_it_created_or_replaced() {
        let engine = testing::engine();
        let router = testing::router(&engine);
        let put =
            |target: &str| testing::call(&router, testing::request(Method::Put, target, SPEC)).0;
        assert_eq!(put("/v1/schedules/tick"), 201);
        assert_eq!(put("/v1/schedules/tick"), 200, "unchanged");
        assert_eq!(put("/v1/schedules/Bad_Id"), 400);
    }

    #[test]
    fn unknown_schedules_are_not_found_everywhere() {
        let engine = testing::engine();
        let router = testing::router(&engine);
        for (method, target) in [
            (Method::Get, "/v1/schedules/none"),
            (Method::Delete, "/v1/schedules/none"),
            (Method::Post, "/v1/schedules/none/pause"),
            (Method::Post, "/v1/schedules/none/resume"),
            (Method::Get, "/v1/schedules/none/preview"),
        ] {
            let label = format!("{method} {target}");
            let (status, body) = testing::call(&router, testing::request(method, target, ""));
            assert_eq!(status, 404, "{label}");
            assert_eq!(body["error"]["code"], "not_found");
        }
    }
}
