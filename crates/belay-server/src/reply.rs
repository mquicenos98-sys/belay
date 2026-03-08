//! Building responses: JSON bodies, error bodies with the right status and
//! `Retry-After`, and the handful of statuses the API uses on success.

use std::time::Duration;

use belay_http::header::{CONTENT_TYPE, LOCATION, RETRY_AFTER};
use belay_http::{Response, StatusCode};
use belay_json::ToJson;
use belay_proto::api::{encode_body, ApiError, ErrorCode};

use crate::rate_limit::retry_after_secs;

/// The media type of every JSON body.
pub(crate) const JSON: &str = "application/json";

/// `status` with `body` serialized as compact JSON.
pub(crate) fn json<T: ToJson + ?Sized>(status: StatusCode, body: &T) -> Response {
    Response::bytes(JSON, encode_body(body)).with_status(status)
}

/// `200 OK` with a JSON body.
pub(crate) fn ok<T: ToJson + ?Sized>(body: &T) -> Response {
    json(StatusCode::OK, body)
}

/// `201 Created` with a JSON body and the new resource's `Location`.
pub(crate) fn created<T: ToJson + ?Sized>(body: &T, location: &str) -> Response {
    json(StatusCode::CREATED, body).header(LOCATION, location)
}

/// `204 No Content`.
pub(crate) fn no_content() -> Response {
    Response::no_content()
}

/// The response for `error`, with the status its code maps to.
///
/// Errors that invite a retry (`unavailable`, `resource_exhausted`) carry
/// `Retry-After: 1` unless the caller sets a better estimate: clients
/// retry sooner or later anyway, and a hint keeps them from hammering a
/// server that is shutting down.
pub(crate) fn error(error: &ApiError) -> Response {
    let response = error_with_status(status_of(error.code), error);
    if error.is_retryable() {
        response.header(RETRY_AFTER, "1")
    } else {
        response
    }
}

/// `error` sent with an explicit status. Only for the few HTTP-level
/// refusals that have no error code of their own (`405`, `413`): their
/// bodies carry `invalid_argument`, which tells a client not to retry the
/// request as is.
pub(crate) fn error_with_status(status: StatusCode, error: &ApiError) -> Response {
    json(status, error)
}
