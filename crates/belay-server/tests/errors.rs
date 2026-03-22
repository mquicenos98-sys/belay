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
