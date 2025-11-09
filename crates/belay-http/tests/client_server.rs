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
