//! Middleware that keeps every answer inside the API's rules: error bodies
//! are always [`ApiError`] JSON, bodies over `server.max_body` are refused
//! with a JSON `413`, and a panicking handler becomes a JSON `500`.
//!
//! The router produces a few answers of its own (`404` for unknown paths,
//! `405` with `Allow`, `400` for a path with broken percent-encoding) as
//! plain text; [`ApiErrors`] rewrites any non-JSON error body into an
//! `ApiError`, keeping the status and headers. `404` becomes `not_found`;
//! `405` and `413` have no error code of their own and carry
//! `invalid_argument`, meaning "do not retry this request as is".

use belay_common::error;
use belay_http::header::{ALLOW, CONTENT_TYPE};
use belay_http::middleware::{CatchPanic, RequestId};
use belay_http::{Body, Middleware, Next, Request, Response, StatusCode};
use belay_json::json;
use belay_proto::api::{encode_body, ApiError, ErrorCode};

use crate::reply::{self, JSON};

/// Rewrites plain-text error answers as `ApiError` JSON.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ApiErrors;

impl Middleware for ApiErrors {
    fn handle(&self, req: &mut Request, next: Next<'_>) -> Response {
        let mut response = next.run(req);
        if response.status.as_u16() < 400 || reply::is_json(&response) {
            return response;
        }
        let error = describe(req, &response);
        response.body = Body::Bytes(encode_body(&error));
        response.headers.insert(CONTENT_TYPE, JSON);
        response
    }
}

/// The `ApiError` for a plain error answer to `req`.
fn describe(req: &Request, response: &Response) -> ApiError {
    let (method, path) = (&req.method, &req.path);
    match response.status {
        StatusCode::NOT_FOUND => ApiError::new(
            ErrorCode::NotFound,
            format!("no route matches {method} {path}"),
        ),
        StatusCode::METHOD_NOT_ALLOWED => {
            let allow: Vec<&str> = response
                .headers
                .get(ALLOW)
                .map(|v| {
                    v.split(',')
                        .map(str::trim)
                        .filter(|m| !m.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            ApiError::invalid_argument(format!(
                "{method} is not allowed on {path}; allowed: {}",
                allow.join(", ")
            ))
            .with_details(json!({ "allow": allow }))
        }
        status => {
            let text = response
                .body_bytes()
                .map(|b| String::from_utf8_lossy(b).trim().to_owned())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| status.to_string());
            ApiError::new(ErrorCode::from_http_status(status.as_u16()), text)
        }
    }
}

/// Refuses bodies larger than `server.max_body`.
///
/// Runs after the HTTP layer has read the body, which it does up to a
/// larger hard limit (see [`read_limit`](crate::options::read_limit)).
#[derive(Clone, Copy, Debug)]
pub(crate) struct BodyLimit {
    max: u64,
}

impl BodyLimit {
    pub(crate) fn new(max: u64) -> BodyLimit {
        BodyLimit { max }
    }
}

impl Middleware for BodyLimit {
    fn handle(&self, req: &mut Request, next: Next<'_>) -> Response {
        let size = req.body.len() as u64;
        if size < self.max {
            return next.run(req);
        }
        let error = ApiError::invalid_argument(format!(
            "request body of {size} bytes exceeds the {} byte limit (server.max_body)",
            self.max
        ))
        .with_details(json!({"limit": self.max, "size": size}));
        reply::error_with_status(StatusCode::CONTENT_TOO_LARGE, &error)
    }
}
