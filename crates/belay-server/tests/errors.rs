//! What the server answers to requests the API cannot serve: unknown
//! routes, wrong methods, oversized bodies, bad paths and parameters. All
//! of them are `ApiError` JSON, with one documented exception.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;

use belay_http::Method;
use belay_proto::api::{decode_error, ErrorCode};
use belay_server::ServerOptions;
use common::{api_error, test_config, TestServer};

#[test]
fn unknown_routes_are_json_not_found() {
    let server = TestServer::start();
    let response = server.get("/v1/nope", None);
    assert_eq!(response.status, 404);
    let error = api_error(&response);
    assert_eq!(error.code, ErrorCode::NotFound);
    assert_eq!(error.message, "no route matches GET /v1/nope");
    // Whatever the method.
    let delete = server.send(Method::Delete, "/nowhere/at/all", "", None);
    assert_eq!(api_error(&delete).code, ErrorCode::NotFound);
}

#[test]
fn wrong_methods_are_405_with_allow_and_a_json_body() {
    let server = TestServer::start();
    let response = server.send(Method::Delete, "/v1/lease", "", None);
    assert_eq!(response.status, 405);
    assert_eq!(response.header("allow"), Some("POST, OPTIONS"));
    let error = api_error(&response);
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert_eq!(
        error.message,
        "DELETE is not allowed on /v1/lease; allowed: POST, OPTIONS"
    );
    let put = server.send(Method::Put, "/v1/jobs", "{}", None);
    assert_eq!(put.status, 405);
    assert_eq!(put.header("allow"), Some("POST, GET, HEAD, OPTIONS"));
}

#[test]
fn bodies_over_the_limit_are_413() {
    let mut config = test_config();
    config.server.max_body = 1024;
    let server = TestServer::with(config, ServerOptions::default());
    let padding = "x".repeat(2048);
    let body = format!(r#"{{"queue": "emails", "kind": "send", "payload": "{padding}"}}"#);
    let response = server.post("/v1/jobs", &body, None);
    assert_eq!(response.status, 413);
    let error = api_error(&response);
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert!(
        error
            .message
            .ends_with("exceeds the 1024 byte limit (server.max_body)"),
        "{}",
        error.message
    );
    assert_eq!(error.details.expect("details")["limit"], 1024);
    // Bodies within the limit are served.
    assert_eq!(
        server
            .post("/v1/jobs", r#"{"queue": "emails", "kind": "send"}"#, None)
            .status,
        201
    );
}

#[test]
fn bodies_far_over_the_limit_are_refused_before_reading() {
    let mut config = test_config();
    config.server.max_body = 1024;
    let server = TestServer::with(config, ServerOptions::default());
    let huge = "x".repeat(200 * 1024);
    let response = server.post("/v1/jobs", &huge, None);
    assert_eq!(response.status, 413);
    // The HTTP layer answers these itself, in plain text.
    assert!(response
        .header("content-type")
        .is_some_and(|t| t.starts_with("text/plain")));
}

#[test]
fn unknown_query_parameters_are_refused_with_a_suggestion() {
    let server = TestServer::start();
    let error = api_error(&server.get("/v1/queues?limti=5", None));
    assert_eq!(
        error.message,
        r#"unknown query parameter "limti", did you mean "limit"?"#
    );
    let flag = api_error(&server.send(Method::Delete, "/v1/queues/q?force=maybe", "", None));
    assert_eq!(
        flag.message,
        r#"$.force: expected true or false, got "maybe""#
    );
    let limit = api_error(&server.get("/v1/workers?limit=5000", None));
    assert_eq!(
        limit.message,
        "$.limit: must be between 1 and 1000, got 5000"
    );
}

#[test]
fn broken_paths_are_json_bad_requests() {
    // Written by hand: an HTTP client would escape the stray `%`.
    let server = TestServer::start();
    let addr = server.handle().local_addr();
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .write_all(b"GET /v1/queues/%zz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .expect("write");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read");
    let (head, body) = response.split_once("\r\n\r\n").expect("head and body");
    assert!(head.starts_with("HTTP/1.1 400 "), "{head}");
    assert!(head.contains("Content-Type: application/json"), "{head}");
    let error = decode_error(400, body.as_bytes());
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert_eq!(error.message, "invalid percent-encoding in path");
}
