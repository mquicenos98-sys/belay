//! The server driven over raw sockets: framing, limits, timeouts and
//! connection management as a client on the wire sees them.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use belay_http::{BodyWriter, Request, Response, Router, ServerConfig, ServerError, StatusCode};
use common::{fast_config, serve, serve_with, Conn};

fn app() -> Router {
    let mut router = Router::new();
    router
        .get("/hello", |_: &mut Request| Response::text("hello"))
        .post("/echo", |req: &mut Request| {
            Response::bytes("application/octet-stream", req.body.clone())
        })
        .get("/peer", |req: &mut Request| {
            Response::text(
                req.remote_addr
                    .map(|a| a.port().to_string())
                    .unwrap_or_default(),
            )
        })
        .get("/path/:id", |req: &mut Request| {
            Response::text(req.param("id").unwrap_or_default().to_owned())
        })
        .get("/stream", |_: &mut Request| {
            Response::stream("text/plain", |out: &mut BodyWriter<'_>| {
                for i in 0..3 {
                    out.send(format!("part{i};").as_bytes())?;
                }
                Ok(())
            })
        })
        .get("/panic", |_: &mut Request| -> Response {
            panic!("handler exploded")
        })
        .get("/bad-header", |_: &mut Request| {
            Response::text("x").header("X-Evil", "a\r\nInjected: 1")
        });
    router
}

fn small_limits() -> ServerConfig {
    ServerConfig {
        max_request_line: 256,
        max_head_bytes: 1024,
        max_headers: 5,
        max_body_bytes: 1024,
        ..fast_config()
    }
}

fn recorded_errors() -> (ServerConfig, Arc<Mutex<Vec<String>>>) {
    let errors = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&errors);
    let config = ServerConfig {
        error_hook: Some(Arc::new(move |e: &ServerError| {
            sink.lock().expect("test lock").push(e.to_string());
        })),
        ..fast_config()
    };
    (config, errors)
}

#[test]
fn serves_a_simple_get_with_standard_headers() {
    let server = serve(app());
    let mut conn = Conn::open(server.local_addr());
    conn.send(b"GET /hello HTTP/1.1\r\nHost: test\r\n\r\n");
    let resp = conn.response();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.text(), "hello");
    assert_eq!(resp.header("content-length"), Some("5"));
    assert_eq!(
        resp.header("content-type"),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(resp.header("server"), Some("belay"));
    let date = resp.header("date").expect("date header");
    assert!(belay_http::parse_http_date(date).is_some(), "{date}");
    assert_eq!(resp.header("connection"), None);
}

#[test]
fn pipelined_requests_are_answered_in_order() {
    let server = serve(app());
    let mut conn = Conn::open(server.local_addr());
    conn.send(
        b"GET /path/1 HTTP/1.1\r\nHost: t\r\n\r\n\
          POST /echo HTTP/1.1\r\nHost: t\r\nContent-Length: 3\r\n\r\nabc\
          GET /path/3 HTTP/1.1\r\nHost: t\r\n\r\n",
    );
    assert_eq!(conn.response().text(), "1");
    assert_eq!(conn.response().text(), "abc");
    assert_eq!(conn.response().text(), "3");
    assert_eq!(server.stats().requests, 3);
}

#[test]
fn keep_alive_reuses_one_connection() {
    let server = serve(app());
    let mut conn = Conn::open(server.local_addr());
    conn.send(b"GET /peer HTTP/1.1\r\nHost: t\r\n\r\n");
    let first = conn.response().text();
    conn.send(b"GET /peer HTTP/1.1\r\nHost: t\r\n\r\n");
    let second = conn.response().text();
    assert_eq!(first, second);
    let stats = server.stats();
    assert_eq!(stats.accepted, 1);
    assert_eq!(stats.requests, 2);
}
