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
