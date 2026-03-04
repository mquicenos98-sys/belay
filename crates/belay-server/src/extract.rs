//! Reading requests: route parameters as typed ids, query strings checked
//! against the parameters an endpoint knows, and JSON bodies.
//!
//! Everything a client can get wrong becomes an `invalid_argument` naming
//! the parameter, never a silently ignored value: a misspelt `limti=5`
//! that is ignored returns a page of 100 and leaves the caller puzzled.
//! Bodies are parsed as JSON whatever `Content-Type` says, so
//! `curl -d '{...}'` (which claims a form) works.

use std::str::FromStr;

use belay_core::Paging;
use belay_http::Request;
use belay_json::{FromJson, Value};
use belay_proto::api::ApiError;
use belay_proto::{JobId, QueueName, ScheduleId, Validate, WorkflowId};

/// The route parameter `name`. The router only calls a handler when its
/// pattern matched, so a missing parameter is a route table bug.
pub(crate) fn param<'r>(req: &'r Request, name: &str) -> Result<&'r str, ApiError> {
    req.param(name)
        .ok_or_else(|| ApiError::internal(format!("route has no :{name} parameter")))
}

/// A route parameter parsed as `T`.
fn parsed<T>(req: &Request, name: &str) -> Result<T, ApiError>
where
    T: FromStr,
    T::Err: Into<ApiError>,
{
    param(req, name)?.parse::<T>().map_err(Into::into)
}

/// The `:id` of a job route.
pub(crate) fn job_id(req: &Request) -> Result<JobId, ApiError> {
    parsed(req, "id")
}

/// The `:name` of a queue route.
pub(crate) fn queue_name(req: &Request) -> Result<QueueName, ApiError> {
    parsed(req, "name")
}

/// The `:id` of a schedule route.
pub(crate) fn schedule_id(req: &Request) -> Result<ScheduleId, ApiError> {
    parsed(req, "id")
}

/// The `:id` of a workflow route.
pub(crate) fn workflow_id(req: &Request) -> Result<WorkflowId, ApiError> {
    parsed(req, "id")
}

/// Refuses query parameters other than `allowed`, suggesting the closest
/// known one for a likely typo.
pub(crate) fn only_params(req: &Request, allowed: &[&str]) -> Result<(), ApiError> {
    for (key, _) in req.query.iter() {
        if allowed.contains(&key) {
            continue;
        }
        let hint = closest(key, allowed)
            .map(|k| format!(", did you mean {k:?}?"))
            .unwrap_or_default();
        return Err(ApiError::invalid_argument(format!(
            "unknown query parameter {key:?}{hint}"
        )));
    }
    Ok(())
}

/// `cursor` and `limit`, plus `extra` parameters the endpoint reads itself.
pub(crate) fn paging(req: &Request, extra: &[&str]) -> Result<Paging, ApiError> {
    let mut allowed = vec!["cursor", "limit"];
    allowed.extend_from_slice(extra);
    only_params(req, &allowed)?;
    Paging::from_pairs(req.query.iter())
}
