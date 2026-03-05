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

/// The body as `T`, whose `field` names the resource: taken from the path
/// when the body leaves it out (so `PUT /v1/calendars/eu` can send just
/// the rules), refused when the body names a different one.
pub(crate) fn body_named<T: FromJson + Validate>(
    req: &Request,
    field: &str,
    path_value: &str,
) -> Result<T, ApiError> {
    let mut value = body_value(req)?;
    let Some(object) = value.as_object_mut() else {
        return Err(ApiError::invalid_argument(
            "request body must be a JSON object",
        ));
    };
    match object.get(field) {
        None => {
            object.insert(field, path_value);
        }
        Some(Value::String(named)) if named == path_value => {}
        Some(other) => {
            let named = other
                .as_str()
                .map_or_else(|| other.to_string(), str::to_owned);
            return Err(ApiError::invalid_argument(format!(
                "$.{field}: body names {named:?} but the path names {path_value:?}"
            )));
        }
    }
    decode(&value)
}

/// The candidate within edit distance 2 of `input`, if any; the closest
/// wins.
fn closest<'a>(input: &str, candidates: &[&'a str]) -> Option<&'a str> {
    candidates
        .iter()
        .map(|c| (edit_distance(input, c), *c))
        .filter(|(d, _)| *d <= 2)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

/// Levenshtein distance over bytes; query keys are ASCII.
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let substitute = prev[j] + usize::from(ca != cb);
            cur[j + 1] = substitute.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_http::Method;
    use belay_proto::api::{ErrorCode, LeaseRequest};
    use belay_proto::CalendarSpec;

    fn get(target: &str) -> Request {
        Request::new(Method::Get, target)
    }

    fn post(body: &str) -> Request {
        Request::new(Method::Post, "/").with_body(body)
    }

    #[test]
    fn unknown_query_parameters_are_refused_with_a_hint() {
        let err = paging(&get("/v1/queues?limti=5"), &[]).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert_eq!(
            err.message,
            r#"unknown query parameter "limti", did you mean "limit"?"#
        );
        let err = paging(&get("/v1/queues?colour=red"), &[]).unwrap_err();
        assert_eq!(err.message, r#"unknown query parameter "colour""#);
        let ok = paging(&get("/v1/queues?limit=5&state=x"), &["state"]).unwrap();
        assert_eq!(ok.limit, Some(5));
    }

    #[test]
    fn flags_accept_the_usual_spellings() {
        for (target, expected) in [
            ("/x", false),
            ("/x?force", true),
            ("/x?force=true", true),
            ("/x?force=1", true),
            ("/x?force=0", false),
            ("/x?force=false", false),
        ] {
            assert_eq!(
                query_flag(&get(target), "force").unwrap(),
                expected,
                "{target}"
            );
        }
        let err = query_flag(&get("/x?force=yes"), "force").unwrap_err();
        assert_eq!(err.message, r#"$.force: expected true or false, got "yes""#);
    }

    #[test]
    fn query_values_are_parsed_and_blank_means_absent() {
        assert_eq!(query_value::<u32>(&get("/x?n=7"), "n").unwrap(), Some(7));
        assert_eq!(query_value::<u32>(&get("/x?n="), "n").unwrap(), None);
        let err = query_value::<u32>(&get("/x?n=seven"), "n").unwrap_err();
        assert!(
            err.message.starts_with("$.n: invalid digit"),
            "{}",
            err.message
        );
    }

    #[test]
    fn bodies_are_parsed_validated_and_explained() {
        let empty = body::<LeaseRequest>(&post("  ")).unwrap_err();
        assert_eq!(
            empty.message,
            "request body is empty; expected a JSON object"
        );
        let malformed = body::<LeaseRequest>(&post("{\"worker\":")).unwrap_err();
        assert!(
            malformed.message.starts_with("invalid JSON: "),
            "{}",
            malformed.message
        );
        let invalid = body::<LeaseRequest>(&post(r#"{"worker":"w","queues":[]}"#)).unwrap_err();
        assert_eq!(invalid.message, "$.queues: must name at least one queue");
        let ok = body::<LeaseRequest>(&post(r#"{"worker":"w","queues":["q"]}"#)).unwrap();
        assert_eq!(ok.queues.len(), 1);
    }

    #[test]
    fn named_bodies_take_the_name_from_the_path() {
        let spec: CalendarSpec = body_named(&post(r#"{"timezone":"UTC"}"#), "name", "eu").unwrap();
        assert_eq!(spec.name, "eu");
        let same: CalendarSpec = body_named(&post(r#"{"name":"eu"}"#), "name", "eu").unwrap();
        assert_eq!(same.name, "eu");
        let err = body_named::<CalendarSpec>(&post(r#"{"name":"us"}"#), "name", "eu").unwrap_err();
        assert_eq!(
            err.message,
            r#"$.name: body names "us" but the path names "eu""#
        );
        let err = body_named::<CalendarSpec>(&post("[1]"), "name", "eu").unwrap_err();
        assert_eq!(err.message, "request body must be a JSON object");
    }

    #[test]
    fn edit_distance_counts_single_character_edits() {
        assert_eq!(edit_distance("limit", "limit"), 0);
        assert_eq!(edit_distance("limti", "limit"), 2);
        assert_eq!(edit_distance("curser", "cursor"), 1);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(closest("stat", &["state", "limit"]), Some("state"));
        assert_eq!(closest("zzzzzz", &["state"]), None);
    }
}
