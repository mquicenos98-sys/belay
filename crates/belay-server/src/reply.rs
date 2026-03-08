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
