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
