//! Graceful shutdown: in-flight requests finish, idle connections and a
//! blocked accept do not hold it up, and the grace period is enforced.

mod common;

use std::net::TcpStream;
use std::sync::mpsc;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use belay_http::{Drain, Request, Response, Router, ServerConfig, ShutdownSignal, StatusCode};
use common::{fast_config, serve, serve_with, Conn};

#[test]
fn shutdown_with_no_connections_returns_promptly() {
    let server = serve(|_: &mut Request| Response::text("unused"));
    // Let the accept thread block in accept() before waking it.
    thread::sleep(Duration::from_millis(20));
    let start = Instant::now();
    assert_eq!(server.shutdown_and_join(), Drain::Complete);
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn in_flight_request_completes_before_shutdown_finishes() {
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let entered_tx = Mutex::new(entered_tx);
    let server = serve(move |_: &mut Request| {
        let _ = entered_tx.lock().expect("test lock").send(());
        thread::sleep(Duration::from_millis(200));
        Response::text("finished")
    });
    let mut conn = Conn::open(server.local_addr());
    conn.send(b"GET /work HTTP/1.1\r\nHost: t\r\n\r\n");
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("handler started");

    server.shutdown();
    let response = conn.response();
    assert_eq!(response.status, 200);
    assert_eq!(response.text(), "finished");
    // The server is draining, so it does not offer another round.
    assert_eq!(response.header("connection"), Some("close"));
    assert!(conn.closed_by_server());
    assert_eq!(server.join(), Drain::Complete);
}

#[test]
fn idle_keep_alive_connections_do_not_delay_shutdown() {
    let config = ServerConfig {
        keep_alive_timeout: Duration::from_secs(60),
        ..fast_config()
    };
    let server = serve_with(config, |_: &mut Request| Response::text("ok"));
    let mut idle: Vec<Conn> = (0..5)
        .map(|_| {
            let mut conn = Conn::open(server.local_addr());
            conn.send(b"GET / HTTP/1.1\r\nHost: t\r\n\r\n");
            assert_eq!(conn.response().status, 200);
            conn
        })
        .collect();
    let start = Instant::now();
    assert_eq!(server.shutdown_and_join(), Drain::Complete);
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
    for conn in &mut idle {
        assert!(conn.closed_by_server());
    }
}

#[test]
fn partially_received_request_is_still_answered() {
    let server = serve(|_: &mut Request| Response::text("late but served"));
    let mut conn = Conn::open(server.local_addr());
    conn.send(b"GET /slowly HTTP/1.1\r\nHo");
    // Make sure the server has buffered the partial head before shutdown.
    let deadline = Instant::now() + Duration::from_secs(2);
    while server.stats().active_connections == 0 {
        assert!(Instant::now() < deadline, "connection never became active");
        thread::sleep(Duration::from_millis(1));
    }
    thread::sleep(Duration::from_millis(30));
    let signal = server.shutdown_signal();
    signal.trigger();
    thread::sleep(Duration::from_millis(50));
    conn.send(b"st: t\r\n\r\n");
    let response = conn.response();
    assert_eq!(response.text(), "late but served");
    assert_eq!(server.join(), Drain::Complete);
}

#[test]
fn new_connections_are_refused_after_shutdown() {
    let server = serve(|_: &mut Request| Response::text("ok"));
    let addr = server.local_addr();
    assert_eq!(server.shutdown_and_join(), Drain::Complete);
    let attempt = TcpStream::connect_timeout(&addr, Duration::from_millis(300));
    assert!(attempt.is_err(), "listener should be closed");
}

#[test]
fn grace_period_bounds_the_wait_for_stuck_handlers() {
    let config = ServerConfig {
        shutdown_grace: Duration::from_millis(100),
        ..fast_config()
    };
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let entered_tx = Mutex::new(entered_tx);
    let server = serve_with(config, move |_: &mut Request| {
        let _ = entered_tx.lock().expect("test lock").send(());
        // Ignores shutdown entirely.
        thread::sleep(Duration::from_millis(1500));
        Response::text("too late")
    });
    let mut conn = Conn::open(server.local_addr());
    conn.send(b"GET / HTTP/1.1\r\nHost: t\r\n\r\n");
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("handler started");
    let start = Instant::now();
    assert_eq!(server.shutdown_and_join(), Drain::TimedOut { remaining: 1 });
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn long_poll_handler_can_return_early_on_shutdown() {
    let mut router = Router::new();
    router.post("/v1/lease", |req: &mut Request| {
        let signal = req
            .extensions
            .get::<ShutdownSignal>()
            .cloned()
            .expect("the server provides its shutdown signal");
        // Wait for work for up to 30 s, as a lease with `wait` would.
        if signal.wait_timeout(Duration::from_secs(30)) {
            return Response::error(StatusCode::SERVICE_UNAVAILABLE, "server shutting down");
        }
        Response::text("[]")
    });
    let server = serve(router);
    let mut conn = Conn::open(server.local_addr());
    conn.send(b"POST /v1/lease HTTP/1.1\r\nHost: t\r\nContent-Length: 2\r\n\r\n{}");
    let deadline = Instant::now() + Duration::from_secs(2);
    while server.stats().requests == 0 {
        assert!(Instant::now() < deadline, "request never arrived");
        thread::sleep(Duration::from_millis(1));
    }
    let start = Instant::now();
    server.shutdown();
    let response = conn.response();
    assert_eq!(response.status, 503);
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(server.join(), Drain::Complete);
}
