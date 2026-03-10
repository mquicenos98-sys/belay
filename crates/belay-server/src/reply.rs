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

/// A `429` for a token over its rate, asking it to come back when the
/// bucket has room.
pub(crate) fn rate_limited(token: &str, wait: Duration) -> Response {
    let secs = retry_after_secs(wait);
    let err = ApiError::resource_exhausted(format!(
        "token {token} is over its request rate; retry in {secs}s"
    ))
    .with_details(belay_json::json!({"retry_after": secs}));
    error_with_status(StatusCode::TOO_MANY_REQUESTS, &err).header(RETRY_AFTER, secs.to_string())
}

/// The status `code` maps to.
pub(crate) fn status_of(code: ErrorCode) -> StatusCode {
    StatusCode::from_u16(code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

/// Whether `response` already carries a JSON body.
pub(crate) fn is_json(response: &Response) -> bool {
    response
        .headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case(JSON))
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_proto::api::decode_error;

    fn body(response: &Response) -> &str {
        std::str::from_utf8(response.body_bytes().expect("in-memory body")).expect("utf-8")
    }

    #[test]
    fn every_error_code_gets_its_documented_status() {
        let cases = [
            (ErrorCode::InvalidArgument, 400),
            (ErrorCode::Unauthenticated, 401),
            (ErrorCode::PermissionDenied, 403),
            (ErrorCode::NotFound, 404),
            (ErrorCode::AlreadyExists, 409),
            (ErrorCode::Conflict, 409),
            (ErrorCode::LeaseLost, 409),
            (ErrorCode::FailedPrecondition, 409),
            (ErrorCode::ResourceExhausted, 429),
            (ErrorCode::Unavailable, 503),
            (ErrorCode::Internal, 500),
        ];
        for (code, status) in cases {
            let response = error(&ApiError::new(code, "x"));
            assert_eq!(response.status, status, "{code}");
            assert!(is_json(&response));
            let decoded = decode_error(status, body(&response).as_bytes());
            assert_eq!(decoded.code, code);
        }
    }

    #[test]
    fn retryable_errors_carry_retry_after() {
        let unavailable = error(&ApiError::unavailable("draining"));
        assert_eq!(unavailable.headers.get("retry-after"), Some("1"));
        let not_found = error(&ApiError::not_found("job", "x"));
        assert_eq!(not_found.headers.get("retry-after"), None);
    }

    #[test]
    fn rate_limit_answers_say_when_to_come_back() {
        let response = rate_limited("ci", Duration::from_millis(2500));
        assert_eq!(response.status, 429);
        assert_eq!(response.headers.get("retry-after"), Some("3"));
        let err = decode_error(429, body(&response).as_bytes());
        assert_eq!(err.code, ErrorCode::ResourceExhausted);
        assert_eq!(
            err.message,
            "token ci is over its request rate; retry in 3s"
        );
    }

    #[test]
    fn created_responses_point_at_the_resource() {
        let response = created(&belay_json::json!({"id": "a"}), "/v1/things/a");
        assert_eq!(response.status, 201);
        assert_eq!(response.headers.get("location"), Some("/v1/things/a"));
        assert_eq!(body(&response), r#"{"id":"a"}"#);
    }

    #[test]
    fn json_detection_ignores_parameters_and_case() {
        let charset = Response::bytes("Application/JSON; charset=utf-8", "{}");
        assert!(is_json(&charset));
        assert!(!is_json(&Response::text("no")));
        assert!(!is_json(&Response::no_content()));
    }
}
