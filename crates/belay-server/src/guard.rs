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
