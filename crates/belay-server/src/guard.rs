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
        if size <= self.max {
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

/// Catches handler panics, logs them with the request id, and answers
/// `internal` without revealing the panic message to the client.
pub(crate) fn catch_panics() -> CatchPanic {
    CatchPanic::new()
        .on_panic(|req, message| {
            let request_id = req.extensions.get::<RequestId>().map(RequestId::as_str);
            error!(
                "handler panicked";
                method = %req.method,
                path = req.path.as_str(),
                request_id = request_id.unwrap_or("-"),
                panic = message,
            );
        })
        .respond_with(|| {
            reply::error(&ApiError::internal(
                "internal error; the server log has the details",
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_common::log::CaptureLogger;
    use belay_http::{Handler, Method, Router};
    use belay_proto::api::decode_error;

    fn app() -> Router {
        let mut router = Router::new();
        router
            .wrap(ApiErrors)
            .wrap(catch_panics())
            .wrap(BodyLimit::new(8))
            .get("/v1/info", |_: &mut Request| Response::text("fine"))
            .post("/v1/jobs", |_: &mut Request| Response::no_content())
            .get("/v1/teapot", |_: &mut Request| {
                Response::error(StatusCode::from_u16(418).expect("valid"), "short and stout")
            })
            .get("/v1/boom", |_: &mut Request| -> Response {
                panic!("kaboom")
            });
        router
    }

    fn call(req: Request) -> (u16, ApiError, Response) {
        let mut req = req;
        let response = app().call(&mut req);
        let status = response.status.as_u16();
        assert!(reply::is_json(&response), "{response:?}");
        let error = decode_error(status, response.body_bytes().expect("json body"));
        (status, error, response)
    }

    #[test]
    fn unknown_routes_are_not_found_errors() {
        let (status, error, _) = call(Request::new(Method::Get, "/v1/nope"));
        assert_eq!(status, 404);
        assert_eq!(error.code, ErrorCode::NotFound);
        assert_eq!(error.message, "no route matches GET /v1/nope");
    }

    #[test]
    fn wrong_methods_list_the_allowed_ones() {
        let (status, error, response) = call(Request::new(Method::Delete, "/v1/info"));
        assert_eq!(status, 405);
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert_eq!(
            error.message,
            "DELETE is not allowed on /v1/info; allowed: GET, HEAD, OPTIONS"
        );
        assert_eq!(response.headers.get("allow"), Some("GET, HEAD, OPTIONS"));
        assert_eq!(error.details.expect("details")["allow"][0], "GET");
    }

    #[test]
    fn oversized_bodies_are_refused_as_json() {
        let req = Request::new(Method::Post, "/v1/jobs").with_body("0123456789");
        let (status, error, _) = call(req);
        assert_eq!(status, 413);
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert_eq!(
            error.message,
            "request body of 10 bytes exceeds the 8 byte limit (server.max_body)"
        );
        let mut small = Request::new(Method::Post, "/v1/jobs").with_body("01234567");
        assert_eq!(app().call(&mut small).status, 204);
    }

    #[test]
    fn other_plain_errors_keep_their_status_and_text() {
        let (status, error, _) = call(Request::new(Method::Get, "/v1/teapot"));
        assert_eq!(status, 418);
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert_eq!(error.message, "short and stout");
        let (status, error, _) = call(Request::new(Method::Get, "/v1/queues/%zz"));
        assert_eq!(status, 400);
        assert_eq!(error.message, "invalid percent-encoding in path");
    }

    #[test]
    fn panics_become_internal_errors_logged_with_the_message() {
        let capture = CaptureLogger::new();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let (status, error, _) = capture.scope(|| call(Request::new(Method::Get, "/v1/boom")));
        std::panic::set_hook(previous);
        assert_eq!(status, 500);
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(!error.message.contains("kaboom"), "{}", error.message);
        let logged = capture
            .find(|r| r.message == "handler panicked")
            .expect("the panic is logged");
        assert_eq!(logged.field("panic"), Some("kaboom"));
        assert_eq!(logged.field("path"), Some("/v1/boom"));
    }

    #[test]
    fn successful_and_json_answers_pass_untouched() {
        let mut req = Request::new(Method::Get, "/v1/info");
        let response = app().call(&mut req);
        assert_eq!(response.body_bytes(), Some(&b"fine"[..]));
    }
}
