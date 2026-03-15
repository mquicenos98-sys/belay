//! `/v1/calendars`: the date and time windows schedules may be restricted
//! to.
//!
//! Calendars are not owned by any one node: each node stores its own, and
//! a schedule is checked against the calendars of the node firing it. In
//! a cluster, put every calendar on every node (the `[[calendars]]`
//! section of a shared configuration file does that).

use belay_core::Upserted;
use belay_http::percent::{encode, EncodeSet};
use belay_http::{Request, Response};
use belay_proto::api::ApiError;
use belay_proto::CalendarSpec;

use crate::app::App;
use crate::extract::{self, only_params};
use crate::reply;

pub(super) fn list(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    let paging = extract::paging(req, &[])?;
    Ok(reply::ok(&app.engine.list_calendars(&paging)?))
}

pub(super) fn get(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    only_params(req, &[])?;
    let name = extract::param(req, "name")?;
    Ok(reply::ok(&app.engine.get_calendar(name)?))
}

/// Creates or replaces a calendar; schedules using it are rescheduled.
/// The body may leave `name` out.
pub(super) fn put(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    only_params(req, &[])?;
    let name = extract::param(req, "name")?.to_owned();
    let spec: CalendarSpec = extract::body_named(req, "name", &name)?;
    let (calendar, outcome) = app.engine.put_calendar(spec)?;
    Ok(match outcome {
        Upserted::Created => {
            // Calendar names are free text; keep a `/` or space in one
            // from changing the shape of the path.
            let segment = encode(&name, EncodeSet::Component);
            reply::created(&calendar, &format!("/v1/calendars/{segment}"))
        }
        Upserted::Updated | Upserted::Unchanged => reply::ok(&calendar),
    })
}

/// Refused while a schedule still uses the calendar.
pub(super) fn delete(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    only_params(req, &[])?;
    let name = extract::param(req, "name")?;
    Ok(reply::ok(&app.engine.delete_calendar(name)?))
}

#[cfg(test)]
mod tests {
    use belay_http::{Handler, Method};

    use crate::routes::testing;

    #[test]
    fn calendars_round_trip_and_report_creation() {
        let engine = testing::engine();
        let router = testing::router(&engine);
        let body = r#"{"timezone": "Europe/Berlin", "exclude_dates": ["2025-12-25"]}"#;
        let mut first = testing::request(Method::Put, "/v1/calendars/holidays", body);
        let created = router.call(&mut first);
        assert_eq!(created.status, 201);
        assert_eq!(
            created.headers.get("location"),
            Some("/v1/calendars/holidays")
        );
        let mut again = testing::request(Method::Put, "/v1/calendars/holidays", body);
        assert_eq!(router.call(&mut again).status, 200);

        let get = || {
            testing::call(
                &router,
                testing::request(Method::Get, "/v1/calendars/holidays", ""),
            )
        };
        let (_, stored) = get();
        assert_eq!(stored["timezone"], "Europe/Berlin");
        assert_eq!(stored["exclude_dates"][0], "2025-12-25");
        let delete = testing::request(Method::Delete, "/v1/calendars/holidays", "");
        assert_eq!(testing::call(&router, delete).0, 200);
        assert_eq!(get().0, 404);
    }
}
