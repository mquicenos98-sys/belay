//! Bearer tokens and scopes as a client sees them, and per-token rate
//! limits.

mod common;

use belay_http::Method;
use belay_proto::api::ErrorCode;
use belay_proto::JobSpec;
use belay_server::{Access, RequestRate, ServerOptions};
use common::{api_error, TestServer, ADMIN, PRODUCER, READER, WORKER};

const JOB: &str = r#"{"queue": "emails", "kind": "send"}"#;
const LEASE: &str = r#"{"worker": "w1", "queues": ["emails"]}"#;

#[test]
fn requests_without_a_valid_token_are_unauthenticated() {
    let server = TestServer::secured(ServerOptions::default());
    let missing = server.get("/v1/info", None);
    assert_eq!(missing.status, 401);
    assert_eq!(api_error(&missing).code, ErrorCode::Unauthenticated);
    assert_eq!(
        missing.header("www-authenticate"),
        Some(r#"Bearer realm="belay""#)
    );

    let wrong = server.get("/v1/info", Some("not-a-configured-token"));
    assert_eq!(wrong.status, 401);
    assert_eq!(
        wrong.header("www-authenticate"),
        Some(r#"Bearer realm="belay", error="invalid_token""#)
    );
    let err = server.client().info().unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::Unauthenticated));
}

#[test]
fn probes_are_public_but_metrics_need_read() {
    let server = TestServer::secured(ServerOptions::default());
    assert_eq!(server.get("/healthz", None).status, 200);
    assert_eq!(server.get("/readyz", None).status, 200);
    assert_eq!(server.get("/metrics", None).status, 401);
    assert_eq!(server.get("/metrics", Some(READER)).status, 200);
}

#[test]
fn each_scope_opens_its_routes_and_nothing_else() {
    let server = TestServer::secured(ServerOptions::default());
    // Producers enqueue but cannot read, work or administer.
    assert_eq!(server.post("/v1/jobs", JOB, Some(PRODUCER)).status, 201);
    assert_eq!(server.get("/v1/jobs", Some(PRODUCER)).status, 403);
    assert_eq!(server.post("/v1/lease", LEASE, Some(PRODUCER)).status, 403);
    // Workers lease and report but cannot enqueue.
    let leased = server.post("/v1/lease", LEASE, Some(WORKER));
    assert_eq!(leased.status, 200);
    let denied = server.post("/v1/jobs", JOB, Some(WORKER));
    assert_eq!(denied.status, 403);
    assert_eq!(
        denied.header("www-authenticate"),
        Some(r#"Bearer realm="belay", error="insufficient_scope", scope="enqueue""#)
    );
    assert_eq!(api_error(&denied).code, ErrorCode::PermissionDenied);
    // Readers read, including events and the members list.
    assert_eq!(server.get("/v1/queues", Some(READER)).status, 200);
    assert_eq!(server.get("/v1/cluster/members", Some(READER)).status, 200);
    assert_eq!(
        server
            .post("/v1/queues/emails/pause", "", Some(READER))
            .status,
        403
    );
    // Administration needs admin, reads included.
    assert_eq!(server.get("/v1/admin/store", Some(READER)).status, 403);
    assert_eq!(server.get("/v1/admin/store", Some(ADMIN)).status, 200);
    assert_eq!(
        server
            .post("/v1/queues/emails/pause", "", Some(ADMIN))
            .status,
        200
    );
}

#[test]
fn admin_tokens_pass_every_route() {
    let server = TestServer::secured(ServerOptions::default());
    let admin = server.client_as(ADMIN);
    let id = admin
        .jobs()
        .enqueue(&JobSpec::new("emails", "send"))
        .unwrap()
        .id;
    admin.jobs().cancel(id).unwrap();
    admin.jobs().retry(id).unwrap();
    admin.info().unwrap();
    admin.admin().checkpoint().unwrap();
}

#[test]
fn the_route_table_matches_what_the_server_enforces() {
    let server = TestServer::secured(ServerOptions::default());
    for route in belay_server::routes() {
        // Concrete paths for the patterns; the ids need not exist, since
        // authorization is checked before anything is looked up.
        let path = route
            .pattern()
            .replace(":id", "01HQ3K9Z8N6V2T4R5S7W0X1Y2Z")
            .replace(":name", "emails");
        if path == "/v1/events" || path == "/v1/admin/shutdown" {
            continue; // a stream and a stop; covered elsewhere
        }
        let anonymous = server.send(route.method(), &path, "", None).status.as_u16();
        match route.access() {
            Access::Public => assert_eq!(anonymous, 200, "{route:?}"),
            Access::Token(_) => assert_eq!(anonymous, 401, "{route:?}"),
        }
        let reader = server.send(route.method(), &path, "", Some(READER));
        let expected_forbidden = !matches!(
            route.access(),
            Access::Public | Access::Token(belay_core::config::Scope::Read)
        );
        assert_eq!(reader.status == 403, expected_forbidden, "{route:?}");
    }
}

#[test]
fn a_token_over_its_rate_gets_429_with_retry_after() {
    let options = ServerOptions::new()
        .with_rate_limit(Some(RequestRate::new(0.5, 2)))
        .with_token_rate_limit("admin", None);
    let server = TestServer::secured(options);
    assert_eq!(server.get("/v1/info", Some(READER)).status, 200);
    assert_eq!(server.get("/v1/info", Some(READER)).status, 200);
    let limited = server.get("/v1/info", Some(READER));
    assert_eq!(limited.status, 429);
    assert_eq!(limited.header("retry-after"), Some("2"));
    let error = api_error(&limited);
    assert_eq!(error.code, ErrorCode::ResourceExhausted);
    assert!(error
        .message
        .starts_with("token reader is over its request rate"));

    // Other tokens have their own buckets, and exempt tokens none.
    assert_eq!(server.post("/v1/lease", LEASE, Some(WORKER)).status, 200);
    for _ in 0..10 {
        assert_eq!(server.get("/v1/info", Some(ADMIN)).status, 200);
    }
    // Probes are never limited.
    for _ in 0..5 {
        assert_eq!(server.get("/healthz", None).status, 200);
    }
    let scrape = server.get("/metrics", Some(ADMIN));
    let text = String::from_utf8_lossy(&scrape.body);
    assert!(
        text.contains("belay_http_rate_limited_total{token=\"reader\"} 1"),
        "{text}"
    );
}

#[test]
fn request_ids_are_echoed_or_minted() {
    let server = TestServer::start();
    let mut req = belay_http::Request::new(Method::Get, server.url("/v1/info"));
    req.headers.insert("X-Request-Id", "trace-42");
    let echoed = common::http().request(req).unwrap();
    assert_eq!(echoed.header("x-request-id"), Some("trace-42"));
    let minted = server.get("/v1/info", None);
    let id = minted.header("x-request-id").expect("an id");
    assert_eq!(id.len(), 16);
}
