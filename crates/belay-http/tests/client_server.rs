//! The client against the server (and against scripted fake servers for
//! the failure modes a real server will not produce on demand).

mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use belay_http::{
    BodyWriter, Client, ClientConfig, Error, Method, RedirectPolicy, Request, RequestOptions,
    Response, Router, StatusCode,
};
use common::{serve, url};

fn echo(req: &mut Request) -> Response {
    let summary = format!("{} {} {}", req.method, req.path, req.body.len());
    Response::text(summary).header("X-Content-Type", req.content_type().unwrap_or("none"))
}

fn api() -> Router {
    let mut router = Router::new();
    for method in [
        Method::Get,
        Method::Post,
        Method::Put,
        Method::Patch,
        Method::Delete,
    ] {
        router.route(method, "/echo", echo);
    }
    router
        .post("/bytes", |req: &mut Request| {
            Response::bytes("application/octet-stream", req.body.clone())
        })
        .get("/peer", |req: &mut Request| {
            Response::text(
                req.remote_addr
                    .map(|a| a.port().to_string())
                    .unwrap_or_default(),
            )
        })
        .get("/connection", |req: &mut Request| {
            Response::text(req.header("connection").unwrap_or("none").to_owned())
        })
        .get("/v1/queues/:name", |req: &mut Request| {
            let state = req.query.get("state").unwrap_or("any").to_owned();
            Response::text(format!(
                "{}|{}",
                req.param("name").unwrap_or_default(),
                state
            ))
        })
        .get("/big/:n", |req: &mut Request| {
            let n: usize = req.param("n").and_then(|n| n.parse().ok()).unwrap_or(0);
            Response::bytes("application/octet-stream", vec![b'z'; n])
        })
        .get("/chunks/:count", |req: &mut Request| {
            let count: usize = req.param("count").and_then(|n| n.parse().ok()).unwrap_or(0);
            Response::stream(
                "application/octet-stream",
                move |out: &mut BodyWriter<'_>| {
                    for i in 0..count {
                        out.send(&[(i % 251) as u8; 64 * 1024])?;
                    }
                    Ok(())
                },
            )
        })
        .get("/slow", |_: &mut Request| {
            thread::sleep(Duration::from_millis(400));
            Response::text("finally")
        })
        .get("/conflict", |_: &mut Request| {
            Response::error(StatusCode::CONFLICT, "queue emails is paused")
        });
    router
}

fn client() -> Client {
    Client::new(ClientConfig::default())
}

#[test]
fn round_trips_every_method() {
    let server = serve(api());
    let client = client();
    let u = url(&server, "/echo");
    assert_eq!(
        client.get(&u, &[]).expect("get").text().ok(),
        Some("GET /echo 0")
    );
    let post = client
        .post(
            &u,
            r#"{"queue":"emails"}"#,
            &[("Content-Type", "application/json")],
        )
        .expect("post");
    assert_eq!(post.text().ok(), Some("POST /echo 18"));
    assert_eq!(post.header("x-content-type"), Some("application/json"));
    assert_eq!(
        client.put(&u, "abc", &[]).expect("put").text().ok(),
        Some("PUT /echo 3")
    );
    assert_eq!(
        client.patch(&u, "", &[]).expect("patch").text().ok(),
        Some("PATCH /echo 0")
    );
    assert_eq!(
        client.delete(&u, &[]).expect("delete").text().ok(),
        Some("DELETE /echo 0")
    );
    // HEAD runs the GET handler (which sees HEAD as the method) and reports
    // the length of "HEAD /echo 0" without sending it.
    let head = client.head(&u, &[]).expect("head");
    assert!(head.body.is_empty());
    assert_eq!(head.header("content-length"), Some("12"));
    // The bodiless response left the connection clean for reuse.
    assert_eq!(
        client.get(&u, &[]).expect("get").text().ok(),
        Some("GET /echo 0")
    );
    assert_eq!(server.stats().accepted, 1);
}

#[test]
fn query_strings_and_encoded_params_reach_the_handler() {
    let server = serve(api());
    let resp = client()
        .get(
            &url(&server, "/v1/queues/team%2Femails?state=ready%20now"),
            &[],
        )
        .expect("get");
    assert_eq!(resp.text().ok(), Some("team/emails|ready now"));
    assert_eq!(resp.url.path(), "/v1/queues/team%2Femails");
}

#[test]
fn pooled_connection_is_reused_across_requests_and_clones() {
    let server = serve(api());
    let client = client();
    let first = client.get(&url(&server, "/peer"), &[]).expect("first");
    assert_eq!(client.idle_connections(), 1);
    let clone = client.clone();
    let second = clone.get(&url(&server, "/peer"), &[]).expect("second");
    assert_eq!(first.text().ok(), second.text().ok());
    assert_eq!(server.stats().accepted, 1);
    client.clear_idle();
    assert_eq!(clone.idle_connections(), 0);
}

#[test]
fn disabled_pooling_asks_the_server_to_close() {
    let server = serve(api());
    let client = Client::new(ClientConfig {
        max_idle_per_host: 0,
        ..ClientConfig::default()
    });
    for _ in 0..2 {
        let resp = client.get(&url(&server, "/connection"), &[]).expect("get");
        assert_eq!(resp.text().ok(), Some("close"));
    }
    assert_eq!(client.idle_connections(), 0);
    assert_eq!(server.stats().accepted, 2);
}

#[test]
fn large_bodies_round_trip() {
    let server = serve(api());
    let client = client();
    let payload: Vec<u8> = (0..3 * 1024 * 1024).map(|i| (i % 253) as u8).collect();
    let resp = client
        .post(&url(&server, "/bytes"), payload.clone(), &[])
        .expect("large post");
    assert_eq!(resp.body.len(), payload.len());
    assert!(resp.body == payload, "echoed body differs");

    let chunked = client
        .get(&url(&server, "/chunks/20"), &[])
        .expect("chunked get");
    assert_eq!(chunked.header("transfer-encoding"), Some("chunked"));
    assert_eq!(chunked.body.len(), 20 * 64 * 1024);
    assert_eq!(chunked.body[64 * 1024 * 3], 3);
    // The chunked response was fully read, so its connection is reusable.
    assert_eq!(client.idle_connections(), 1);
}

#[test]
fn streaming_response_is_readable_before_it_ends() {
    let mut router = Router::new();
    let (hold_tx, hold_rx) = mpsc::channel::<()>();
    let hold_rx = Arc::new(Mutex::new(hold_rx));
    router.get("/held", move |_: &mut Request| {
        let hold_rx = Arc::clone(&hold_rx);
        Response::stream("text/plain", move |out: &mut BodyWriter<'_>| {
            out.send(b"first;")?;
            let _ = hold_rx
                .lock()
                .expect("test lock")
                .recv_timeout(Duration::from_secs(5));
            out.send(b"second;")?;
            Ok(())
        })
    });
    let server = serve(router);
    let client = client();
    let mut resp = client
        .stream(Request::new(Method::Get, url(&server, "/held")))
        .expect("stream");
    let mut buf = [0u8; 64];
    let n = resp.body.read(&mut buf).expect("first chunk");
    assert_eq!(&buf[..n], b"first;");
    assert!(!resp.body.is_complete());
    hold_tx.send(()).expect("server waiting");
    let rest = resp.collect().expect("rest of body");
    assert_eq!(rest.text().ok(), Some("second;"));
}

#[test]
fn read_timeout_is_reported_and_can_be_extended_per_request() {
    let server = serve(api());
    let client = Client::new(ClientConfig {
        read_timeout: Some(Duration::from_millis(100)),
        ..ClientConfig::default()
    });
    let err = client
        .get(&url(&server, "/slow"), &[])
        .expect_err("too slow");
    assert!(err.is_timeout(), "{err}");

    let options = RequestOptions {
        read_timeout: Some(Duration::from_secs(5)),
        ..RequestOptions::default()
    };
    let resp = client
        .request_with(Request::new(Method::Get, url(&server, "/slow")), &options)
        .expect("long-poll style request");
    assert_eq!(resp.text().ok(), Some("finally"));
}

#[test]
fn response_size_limit_is_enforced() {
    let server = serve(api());
    let client = Client::new(ClientConfig {
        max_response_bytes: 1000,
        ..ClientConfig::default()
    });
    assert!(client.get(&url(&server, "/big/1000"), &[]).is_ok());
    for path in ["/big/1001", "/chunks/1"] {
        let err = client
            .get(&url(&server, path), &[])
            .expect_err("over the limit");
        assert!(
            matches!(err, Error::BodyTooLarge { limit: 1000 }),
            "{path}: {err}"
        );
    }
}

#[test]
fn error_for_status_surfaces_the_server_message() {
    let server = serve(api());
    let err = client()
        .get(&url(&server, "/conflict"), &[])
        .and_then(|r| r.error_for_status())
        .expect_err("409");
    assert_eq!(
        err.to_string(),
        "server responded 409 Conflict: queue emails is paused"
    );
}

fn redirecting_server(other: Option<SocketAddr>) -> belay_http::ServerHandle {
    let mut router = Router::new();
    router
        .get("/hop/:n", |req: &mut Request| {
            let n: u32 = req.param("n").and_then(|n| n.parse().ok()).unwrap_or(0);
            if n == 0 {
                Response::text("arrived")
            } else {
                Response::redirect(StatusCode::TEMPORARY_REDIRECT, &format!("/hop/{}", n - 1))
            }
        })
        .post("/submit", |_: &mut Request| {
            Response::redirect(StatusCode::SEE_OTHER, "landing?from=submit")
        })
        .post("/moved", |_: &mut Request| {
            Response::redirect(StatusCode::PERMANENT_REDIRECT, "/landing")
        })
        .get("/landing", echo)
        .post("/landing", echo)
        .get("/auth", |req: &mut Request| {
            Response::text(req.header("authorization").unwrap_or("none").to_owned())
        });
    if let Some(other) = other {
        router.get("/elsewhere", move |_: &mut Request| {
            Response::redirect(StatusCode::FOUND, &format!("http://{other}/auth"))
        });
    }
    serve(router)
}

#[test]
fn redirects_are_returned_unless_enabled() {
    let server = redirecting_server(None);
    let resp = client().get(&url(&server, "/hop/2"), &[]).expect("get");
    assert_eq!(resp.status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(resp.header("location"), Some("/hop/1"));

    let following = Client::new(ClientConfig {
        redirects: RedirectPolicy::limited(5),
        ..ClientConfig::default()
    });
    let resp = following
        .get(&url(&server, "/hop/3"), &[])
        .expect("followed");
    assert_eq!(resp.text().ok(), Some("arrived"));
    assert_eq!(resp.url.path(), "/hop/0");
    // Every hop reused the same pooled connection.
    assert_eq!(server.stats().accepted, 2);
}

#[test]
fn redirect_hop_limit_is_enforced() {
    let server = redirecting_server(None);
    let client = Client::new(ClientConfig {
        redirects: RedirectPolicy::limited(2),
        ..ClientConfig::default()
    });
    assert!(client.get(&url(&server, "/hop/2"), &[]).is_ok());
    let err = client
        .get(&url(&server, "/hop/3"), &[])
        .expect_err("three hops");
    assert!(matches!(err, Error::TooManyRedirects { max: 2 }), "{err}");
}

#[test]
fn see_other_turns_post_into_get_but_308_keeps_it() {
    let server = redirecting_server(None);
    let client = Client::new(ClientConfig {
        redirects: RedirectPolicy::limited(3),
        ..ClientConfig::default()
    });
    let resp = client
        .post(
            &url(&server, "/submit"),
            "payload",
            &[("Content-Type", "text/plain")],
        )
        .expect("303");
    assert_eq!(resp.text().ok(), Some("GET /landing 0"));
    assert_eq!(resp.header("x-content-type"), Some("none"));
    assert_eq!(resp.url.query(), Some("from=submit"));

    let resp = client
        .post(&url(&server, "/moved"), "payload", &[])
        .expect("308");
    assert_eq!(resp.text().ok(), Some("POST /landing 7"));
}

#[test]
fn credentials_do_not_follow_cross_origin_redirects_by_default() {
    let target = redirecting_server(None);
    let origin = redirecting_server(Some(target.local_addr()));
    let auth = [("Authorization", "Bearer s3cret")];

    let cautious = Client::new(ClientConfig {
        redirects: RedirectPolicy::limited(1),
        ..ClientConfig::default()
    });
    let resp = cautious
        .get(&url(&origin, "/elsewhere"), &auth)
        .expect("followed");
    assert_eq!(resp.text().ok(), Some("none"));

    let trusting = Client::new(ClientConfig {
        redirects: RedirectPolicy::limited(1).with_forwarded_credentials(true),
        ..ClientConfig::default()
    });
    let resp = trusting
        .get(&url(&origin, "/elsewhere"), &auth)
        .expect("followed");
    assert_eq!(resp.text().ok(), Some("Bearer s3cret"));
}

/// Reads one request (head plus `Content-Length` body) from a raw socket.
fn read_request(stream: &mut TcpStream) -> Option<String> {
    let mut data = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => data.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
    let length: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    while data.len() < head_end + length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => data.extend_from_slice(&chunk[..n]),
        }
    }
    Some(head)
}

/// A server that answers the first request on its first connection with
/// keep-alive, then closes that connection when the second request arrives
/// without answering it: exactly what a server that timed out the idle
/// connection concurrently with the client's reuse looks like. Later
/// connections are answered normally. Returns the number of connections.
fn closes_reused_connections(connections: usize) -> (SocketAddr, JoinHandle<usize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let handle = thread::spawn(move || {
        let (mut first, _) = listener.accept().expect("first connection");
        read_request(&mut first).expect("first request");
        first
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nfirst")
            .expect("respond");
        read_request(&mut first);
        drop(first);
        for _ in 1..connections {
            let (mut next, _) = listener.accept().expect("retry connection");
            read_request(&mut next).expect("retried request");
            next.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nsecond",
            )
            .expect("respond");
        }
        connections
    });
    (addr, handle)
}

#[test]
fn idempotent_request_is_retried_on_a_stale_pooled_connection() {
    let (addr, server) = closes_reused_connections(2);
    let client = client();
    let base = format!("http://{addr}");
    assert_eq!(
        client
            .get(&format!("{base}/a"), &[])
            .expect("first")
            .text()
            .ok(),
        Some("first")
    );
    assert_eq!(client.idle_connections(), 1);
    let resp = client
        .get(&format!("{base}/b"), &[])
        .expect("retried transparently");
    assert_eq!(resp.text().ok(), Some("second"));
    assert_eq!(server.join().expect("fake server"), 2);
}

#[test]
fn non_idempotent_request_is_not_retried() {
    let (addr, server) = closes_reused_connections(1);
    let client = client();
    let base = format!("http://{addr}");
    client.get(&format!("{base}/a"), &[]).expect("first");
    let err = client
        .post(&format!("{base}/jobs"), "{}", &[])
        .expect_err("the server may have acted on the post");
    assert!(matches!(err, Error::ConnectionClosed), "{err}");
    assert_eq!(server.join().expect("fake server"), 1);
}

#[test]
fn truncated_response_body_is_an_error() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = thread::spawn(move || {
        let (mut conn, _) = listener.accept().expect("connection");
        read_request(&mut conn);
        let _ = conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nonly a little");
    });
    let err = client()
        .get(&format!("http://{addr}/"), &[])
        .expect_err("truncated");
    assert!(matches!(err, Error::ConnectionClosed), "{err}");
    server.join().expect("fake server");
}

#[test]
fn interim_responses_are_skipped() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = thread::spawn(move || {
        let (mut conn, _) = listener.accept().expect("connection");
        read_request(&mut conn);
        let _ = conn.write_all(
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 103 Early Hints\r\nLink: </a>\r\n\r\n\
              HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\nok",
        );
    });
    let resp = client()
        .post(&format!("http://{addr}/"), "x", &[])
        .expect("final response");
    assert_eq!(resp.status, StatusCode::CREATED);
    assert_eq!(resp.text().ok(), Some("ok"));
    server.join().expect("fake server");
}

#[test]
fn close_delimited_response_is_read_to_eof_and_not_pooled() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = thread::spawn(move || {
        let (mut conn, _) = listener.accept().expect("connection");
        read_request(&mut conn);
        let _ = conn.write_all(b"HTTP/1.0 200 OK\r\n\r\nall of it");
    });
    let client = client();
    let resp = client
        .get(&format!("http://{addr}/"), &[])
        .expect("response");
    assert_eq!(resp.text().ok(), Some("all of it"));
    assert_eq!(client.idle_connections(), 0);
    server.join().expect("fake server");
}
