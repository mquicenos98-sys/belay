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

/// The query parameter `key` parsed as `T`, if present and not empty.
pub(crate) fn query_value<T>(req: &Request, key: &str) -> Result<Option<T>, ApiError>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    match req.query.get(key).map(str::trim) {
        None | Some("") => Ok(None),
        Some(text) => text
            .parse::<T>()
            .map(Some)
            .map_err(|e| ApiError::invalid_argument(format!("$.{key}: {e}"))),
    }
}

/// A boolean query flag: `true`/`false`, `1`/`0`, or present without a
/// value (`?force`), which means true.
pub(crate) fn query_flag(req: &Request, key: &str) -> Result<bool, ApiError> {
    match req.query.get(key).map(str::trim) {
        None => Ok(false),
        Some("" | "true" | "1") => Ok(true),
        Some("false" | "0") => Ok(false),
        Some(other) => Err(ApiError::invalid_argument(format!(
            "$.{key}: expected true or false, got {other:?}"
        ))),
    }
}

/// The body as a JSON value; an empty body is refused with a message that
/// says so rather than a parser position.
pub(crate) fn body_value(req: &Request) -> Result<Value, ApiError> {
    if req.body.iter().all(u8::is_ascii_whitespace) {
        return Err(ApiError::invalid_argument(
            "request body is empty; expected a JSON object",
        ));
    }
    belay_json::parse_bytes(&req.body).map_err(|e| ApiError::from(belay_json::Error::from(e)))
}

/// Decodes and validates an already parsed body.
pub(crate) fn decode<T: FromJson + Validate>(value: &Value) -> Result<T, ApiError> {
    let decoded = T::from_json(value)?;
    decoded.validate()?;
    Ok(decoded)
}

/// The body decoded and validated as `T`.
pub(crate) fn body<T: FromJson + Validate>(req: &Request) -> Result<T, ApiError> {
    decode(&body_value(req)?)
}
