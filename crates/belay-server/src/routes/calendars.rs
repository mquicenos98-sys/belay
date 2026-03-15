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
