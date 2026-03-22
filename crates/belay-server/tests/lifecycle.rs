//! Starting and stopping: binding the configured address, readiness,
//! graceful shutdown through the handle and through the admin endpoint,
//! and long polls that must not hold a shutdown up.

mod common;

use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use belay_core::Engine;
use belay_proto::api::{ErrorCode, LeaseRequest};
use belay_proto::{QueueName, WorkerId};
use belay_server::{Error, ServerOptions};
use common::{api_error, json, quiet_logs, test_config, TestServer, ADMIN, READER};

#[test]
fn the_admin_endpoint_shuts_the_server_down_gracefully() {
    let mut server = TestServer::secured(ServerOptions::default());
    let addr = server.handle().local_addr();

    let denied = server.post("/v1/admin/shutdown", "", Some(READER));
    assert_eq!(denied.status, 403);
    assert!(!server.handle().is_shutting_down());

    let accepted = server.post("/v1/admin/shutdown", "", Some(ADMIN));
    assert_eq!(accepted.status, 202);
    let body = json(&accepted);
    assert_eq!(body["shutting_down"], true);
    assert_eq!(body["grace"], "5s");

    let started = Instant::now();
    server.take_handle().wait();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(server.engine.is_shutting_down());
    assert!(TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_err());
}

#[test]
fn a_waiting_long_poll_does_not_hold_up_a_shutdown() {
    let mut server = TestServer::start();
    let client = server.client();
    let poll = thread::spawn(move || {
        let request = LeaseRequest::new(
            WorkerId::from_static("w1"),
            vec![QueueName::from_static("emails")],
        )
        .wait(Duration::from_secs(30));
        client.jobs().lease(&request)
    });
    assert!(common::eventually(Duration::from_secs(5), || {
        server
            .engine
            .render_metrics()
            .contains("belay_lease_waiting 1")
    }));
    let started = Instant::now();
    server.take_handle().shutdown_and_join();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(3),
        "shutdown took {elapsed:?}"
    );
    // The poll was answered, with nothing, rather than cut off.
    let jobs = poll.join().expect("no panic").expect("answered");
    assert!(jobs.is_empty());
}

#[test]
fn readiness_follows_the_engine() {
    let server = TestServer::start();
    let client = server.client();
    assert!(client.ready().unwrap());
    server.engine.shutdown();
    assert!(!client.ready().unwrap());
    let response = server.get("/readyz", None);
    assert_eq!(response.status, 503);
    assert_eq!(response.header("retry-after"), Some("1"));
    assert_eq!(api_error(&response).message, "shutting down");
    // Liveness is unaffected: the process still answers.
    client.health().unwrap();
    // Calls that need the engine say it is unavailable.
    let enqueue = server.post("/v1/jobs", r#"{"queue": "q", "kind": "k"}"#, None);
    assert_eq!(api_error(&enqueue).code, ErrorCode::Unavailable);
}

fn engine_listening_on(listen: &str) -> Arc<Engine> {
    quiet_logs();
    let mut config = test_config();
    config.server.listen = listen.to_owned();
    let engine = Engine::builder()
        .config(config)
        .background(false)
        .start()
        .expect("engine starts");
    Arc::new(engine)
}

#[test]
fn serve_binds_the_configured_address() {
    let engine = engine_listening_on("127.0.0.1:0");
    let handle = belay_server::serve(Arc::clone(&engine)).expect("binds");
    let addr = handle.local_addr();
    assert_ne!(addr.port(), 0);
    let response = common::http()
        .get(&format!("http://{addr}/healthz"), &[])
        .expect("answers");
    assert_eq!(response.status, 200);
    assert_eq!(response.text().unwrap(), "ok\n");
    assert!(response
        .header("server")
        .is_some_and(|s| s.starts_with("belay/")));
    handle.shutdown_and_join();
    assert!(engine.is_shutting_down());
}

#[test]
fn an_address_in_use_is_a_bind_error() {
    let taken = TcpListener::bind("127.0.0.1:0").expect("ephemeral port");
    let addr = taken.local_addr().expect("bound").to_string();
    let engine = engine_listening_on(&addr);
    match belay_server::serve(engine) {
        Err(Error::Bind {
            addr: reported,
            source,
        }) => {
            assert_eq!(reported, addr);
            assert_eq!(source.kind(), std::io::ErrorKind::AddrInUse);
        }
        Err(other) => panic!("expected a bind error, got {other}"),
        Ok(_) => panic!("bound an address that is in use"),
    }
}

#[test]
fn routers_can_be_embedded_without_a_server() {
    use belay_http::{Handler, Method, Request};
    let engine = engine_listening_on("127.0.0.1:0");
    let router = belay_server::router(Arc::clone(&engine));
    let mut req =
        Request::new(Method::Post, "/v1/jobs").with_body(r#"{"queue": "emails", "kind": "send"}"#);
    assert_eq!(router.call(&mut req).status, 201);
    // Without a server there is nothing for the admin endpoint to stop.
    let mut stop = Request::new(Method::Post, "/v1/admin/shutdown");
    let refused = router.call(&mut stop);
    assert_eq!(refused.status, 409);
    engine.shutdown();
}

#[test]
fn long_polls_hold_connection_slots_until_capacity_refuses_more() {
    let mut config = test_config();
    config.server.max_connections = 2;
    let server = TestServer::with(config, ServerOptions::default());
    // Two workers, each on its own connection, poll an empty queue.
    let polls: Vec<_> = (0..2)
        .map(|_| {
            let url = server.url("/v1/lease");
            thread::spawn(move || {
                let body = r#"{"worker": "w", "queues": ["emails"], "wait": "20s"}"#;
                common::http().post(&url, body, &[]).expect("answered")
            })
        })
        .collect();
    assert!(common::eventually(Duration::from_secs(5), || {
        server
            .engine
            .render_metrics()
            .contains("belay_lease_waiting 2")
    }));

    // Both slots are taken: the HTTP layer turns the next connection away.
    let refused = server.get("/healthz", None);
    assert_eq!(refused.status, 503);
    assert_eq!(refused.header("retry-after"), Some("1"));

    // Work arrives, the polls return, and the slots come free again.
    for _ in 0..2 {
        server
            .engine
            .enqueue(belay_proto::JobSpec::new("emails", "send"))
            .unwrap();
    }
    for poll in polls {
        let answer = poll.join().expect("no panic");
        assert_eq!(answer.status, 200);
        assert_eq!(json(&answer)["jobs"].as_array().map(Vec::len), Some(1));
    }
    assert!(common::eventually(Duration::from_secs(5), || {
        server.get("/healthz", None).status == 200
    }));
}
