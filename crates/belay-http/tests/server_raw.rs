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

#[test]
fn request_arriving_one_byte_at_a_time() {
    let server = serve(app());
    let mut conn = Conn::open(server.local_addr());
    let request = b"POST /echo HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nwiki\r\n0\r\n\r\n";
    for byte in request.iter() {
        conn.send(std::slice::from_ref(byte));
    }
    let resp = conn.response();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.text(), "wiki");
}

#[test]
fn leading_blank_lines_are_ignored() {
    let server = serve(app());
    let mut conn = Conn::open(server.local_addr());
    conn.send(b"\r\n\r\nGET /hello HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(conn.response().status, 200);
}

#[test]
fn absolute_form_and_encoded_paths_route_normally() {
    let server = serve(app());
    let mut conn = Conn::open(server.local_addr());
    conn.send(b"GET http://example.com/path/9?x=1 HTTP/1.1\r\nHost: example.com\r\n\r\n");
    assert_eq!(conn.response().text(), "9");
    conn.send(b"GET /path/a%20b HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(conn.response().text(), "a b");
}

#[test]
fn long_request_line_gets_414() {
    let server = serve_with(small_limits(), app());
    let mut conn = Conn::open(server.local_addr());
    let mut request = b"GET /".to_vec();
    request.extend(std::iter::repeat(b'a').take(300));
    request.extend_from_slice(b" HTTP/1.1\r\nHost: t\r\n\r\n");
    conn.send(&request);
    assert_eq!(conn.response().status, 414);
}

#[test]
fn too_many_header_fields_get_431() {
    let server = serve_with(small_limits(), app());
    let mut conn = Conn::open(server.local_addr());
    conn.send(b"GET /hello HTTP/1.1\r\nHost: t\r\nA: 1\r\nB: 2\r\nC: 3\r\nD: 4\r\nE: 5\r\n\r\n");
    assert_eq!(conn.response().status, 431);
}

#[test]
fn declared_oversized_body_is_refused_before_it_is_sent() {
    let server = serve_with(small_limits(), app());
    let mut conn = Conn::open(server.local_addr());
    // Only the head is sent; the 413 must not wait for 10 MiB of body.
    conn.send(b"POST /echo HTTP/1.1\r\nHost: t\r\nContent-Length: 10485760\r\n\r\n");
    let resp = conn.response();
    assert_eq!(resp.status, 413);
    assert!(
        resp.text().contains("exceeds the 1024 byte limit"),
        "{}",
        resp.text()
    );
}

#[test]
fn oversized_chunked_body_gets_413() {
    let server = serve_with(small_limits(), app());
    let mut conn = Conn::open(server.local_addr());
    let mut request =
        b"POST /echo HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n320\r\n".to_vec();
    request.extend(std::iter::repeat(b'x').take(0x320));
    request.extend_from_slice(b"\r\n320\r\n");
    conn.send(&request);
    assert_eq!(conn.response().status, 413);
}

#[test]
fn malformed_chunk_gets_400() {
    let server = serve(app());
    let mut conn = Conn::open(server.local_addr());
    conn.send(b"POST /echo HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\nabc\r\n");
    let resp = conn.response();
    assert_eq!(resp.status, 400);
    assert!(resp.text().contains("chunk"), "{}", resp.text());
}

#[test]
fn chunked_body_with_extensions_and_trailers_is_decoded() {
    let server = serve(app());
    let mut conn = Conn::open(server.local_addr());
    conn.send(
        b"POST /echo HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n\
          3\r\nabc\r\n2;name=v\r\nde\r\n0\r\nX-Checksum: 1\r\n\r\n",
    );
    assert_eq!(conn.response().text(), "abcde");
}

#[test]
fn slow_request_head_gets_408() {
    let config = ServerConfig {
        header_timeout: Duration::from_millis(200),
        ..fast_config()
    };
    let server = serve_with(config, app());
    let mut conn = Conn::open(server.local_addr());
    let start = Instant::now();
    conn.send(b"GET /hello HTTP/1.1\r\nHo");
    let resp = conn.response();
    assert_eq!(resp.status, 408);
    let elapsed = start.elapsed();
    assert!(elapsed >= Duration::from_millis(150), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
}

#[test]
fn slow_request_body_gets_408() {
    let config = ServerConfig {
        body_timeout: Duration::from_millis(200),
        ..fast_config()
    };
    let server = serve_with(config, app());
    let mut conn = Conn::open(server.local_addr());
    conn.send(b"POST /echo HTTP/1.1\r\nHost: t\r\nContent-Length: 10\r\n\r\nabc");
    assert_eq!(conn.response().status, 408);
}

#[test]
fn silent_connection_is_closed_without_a_response() {
    let config = ServerConfig {
        header_timeout: Duration::from_millis(150),
        ..fast_config()
    };
    let server = serve_with(config, app());
    let mut conn = Conn::open(server.local_addr());
    let start = Instant::now();
    assert!(conn.closed_by_server());
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn idle_keep_alive_connection_times_out() {
    let config = ServerConfig {
        keep_alive_timeout: Duration::from_millis(150),
        ..fast_config()
    };
    let server = serve_with(config, app());
    let mut conn = Conn::open(server.local_addr());
    conn.send(b"GET /hello HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(conn.response().status, 200);
    let start = Instant::now();
    assert!(conn.closed_by_server());
    let elapsed = start.elapsed();
    assert!(elapsed >= Duration::from_millis(100), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
}

#[test]
fn head_sends_length_but_no_body() {
    let server = serve(app());
    let mut conn = Conn::open(server.local_addr());
    conn.send(b"HEAD /hello HTTP/1.1\r\nHost: t\r\n\r\n");
    let head = conn.head();
    assert_eq!(head.status, 200);
    assert_eq!(head.header("content-length"), Some("5"));
    // Stray body bytes would be parsed as the start of this response.
    conn.send(b"GET /path/2 HTTP/1.1\r\nHost: t\r\n\r\n");
    let next = conn.response();
    assert_eq!(next.status, 200);
    assert_eq!(next.text(), "2");
}

#[test]
fn head_of_a_stream_is_not_streamed() {
    let server = serve(app());
    let mut conn = Conn::open(server.local_addr());
    conn.send(b"HEAD /stream HTTP/1.1\r\nHost: t\r\n\r\n");
    let head = conn.head();
    assert_eq!(head.header("transfer-encoding"), Some("chunked"));
    conn.send(b"GET /hello HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(conn.response().text(), "hello");
}

#[test]
fn expect_100_continue_gets_an_interim_response() {
    let server = serve(app());
    let mut conn = Conn::open(server.local_addr());
    conn.send(
        b"POST /echo HTTP/1.1\r\nHost: t\r\nContent-Length: 5\r\nExpect: 100-continue\r\n\r\n",
    );
    let interim = conn.head();
    assert_eq!(interim.status, 100);
    conn.send(b"hello");
    let resp = conn.response();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.text(), "hello");
}

#[test]
fn expect_100_continue_with_oversized_body_gets_413_directly() {
    let server = serve_with(small_limits(), app());
    let mut conn = Conn::open(server.local_addr());
    conn.send(
        b"POST /echo HTTP/1.1\r\nHost: t\r\nContent-Length: 5000\r\nExpect: 100-continue\r\n\r\n",
    );
    assert_eq!(conn.head().status, 413);
}

#[test]
fn unknown_expectation_gets_417() {
    let server = serve(app());
    let mut conn = Conn::open(server.local_addr());
    conn.send(b"POST /echo HTTP/1.1\r\nHost: t\r\nContent-Length: 1\r\nExpect: telepathy\r\n\r\nx");
    assert_eq!(conn.response().status, 417);
}

#[test]
fn http10_closes_unless_keep_alive_is_requested() {
    let server = serve(app());
    let mut conn = Conn::open(server.local_addr());
    conn.send(b"GET /hello HTTP/1.0\r\n\r\n");
    let resp = conn.response();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.header("connection"), Some("close"));
    assert!(conn.closed_by_server());

    let mut conn = Conn::open(server.local_addr());
    conn.send(b"GET /hello HTTP/1.0\r\nConnection: keep-alive\r\n\r\n");
    let resp = conn.response();
    assert_eq!(resp.header("connection"), Some("keep-alive"));
    conn.send(b"GET /path/5 HTTP/1.0\r\nConnection: keep-alive\r\n\r\n");
    assert_eq!(conn.response().text(), "5");
}

#[test]
fn connection_close_is_honoured() {
    let server = serve(app());
    let mut conn = Conn::open(server.local_addr());
    conn.send(b"GET /hello HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n");
    let resp = conn.response();
    assert_eq!(resp.header("connection"), Some("close"));
    assert!(conn.closed_by_server());
}

#[test]
fn max_requests_per_connection_closes_after_the_last_one() {
    let config = ServerConfig {
        max_requests_per_connection: 2,
        ..fast_config()
    };
    let server = serve_with(config, app());
    let mut conn = Conn::open(server.local_addr());
    conn.send(b"GET /hello HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(conn.response().header("connection"), None);
    conn.send(b"GET /hello HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(conn.response().header("connection"), Some("close"));
    assert!(conn.closed_by_server());
}

#[test]
fn protocol_violations_get_specific_statuses() {
    let server = serve(app());
    let cases: [(&[u8], u16); 8] = [
        (b"GET /hello HTTP/1.1\r\n\r\n", 400),
        (b"GET /hello HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n", 400),
        (b"POST /echo HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: gzip, chunked\r\n\r\n", 501),
        (b"POST /echo HTTP/1.1\r\nHost: t\r\nContent-Length: 1, 2\r\n\r\n", 400),
        (b"GET /hello HTTP/2.0\r\nHost: t\r\n\r\n", 505),
        (b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n", 501),
        (b"GET * HTTP/1.1\r\nHost: t\r\n\r\n", 400),
        (b"GET /hello HTTP/1.1\r\nHost: t\r\nBad Header: x\r\n\r\n", 400),
    ];
    for (request, status) in cases {
        let mut conn = Conn::open(server.local_addr());
        conn.send(request);
        let resp = conn.response();
        assert_eq!(resp.status, status, "{}", String::from_utf8_lossy(request));
        assert_eq!(resp.header("connection"), Some("close"));
    }
    assert_eq!(server.stats().protocol_errors, cases.len() as u64);
}
