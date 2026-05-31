//! `GET /v1/events` over the network: framing, resuming with
//! `Last-Event-ID`, `retry:`, heartbeats, lag frames, filters, and how
//! streams end.

mod common;

use std::io::Read;
use std::time::{Duration, Instant};

use belay_client::EventOptions;
use belay_http::{EventStream, Method, Request, SseEvent, SseOptions};
use belay_proto::api::ErrorCode;
use belay_proto::event::{EventFilter, EventKind};
use belay_proto::JobSpec;
use belay_server::ServerOptions;
use common::{api_error, http, test_config, TestServer};

fn enqueue(server: &TestServer, queue: &'static str) {
    server
        .engine
        .enqueue(JobSpec::new(queue, "send"))
        .expect("enqueue");
}

fn open(server: &TestServer, query: &str, last_event_id: Option<u64>) -> EventStream {
    let mut options = SseOptions::default()
        .max_reconnects(0)
        .read_timeout(Some(Duration::from_secs(5)));
    if let Some(id) = last_event_id {
        options = options.last_event_id(id.to_string());
    }
    http()
        .sse_with(&server.url(&format!("/v1/events{query}")), options)
        .expect("stream opens")
}

fn next(stream: &mut EventStream) -> SseEvent {
    stream
        .next()
        .expect("the stream is open")
        .expect("an event arrives")
}

#[test]
fn the_typed_client_follows_the_stream() {
    let server = TestServer::start();
    let first = server.engine.events().next_seq();
    enqueue(&server, "emails");
    let filter = EventFilter::new().from_seq(first);
    let options = EventOptions::default()
        .idle_timeout(Duration::from_secs(5))
        .max_reconnects(0);
    let mut events = server.client().events_with(&filter, options).unwrap();
    let event = events.next().expect("an event").expect("decodes");
    assert_eq!(event.seq, first);
    assert!(matches!(event.kind, EventKind::JobEnqueued { .. }));
    enqueue(&server, "sms");
    let live = events.next().expect("a live event").expect("decodes");
    assert_eq!(live.seq, first + 1);
}

#[test]
fn frames_carry_id_type_and_json_after_a_retry_hint() {
    let server = TestServer::start();
    let first = server.engine.events().next_seq();
    enqueue(&server, "emails");
    let mut stream = open(&server, &format!("?from={first}"), None);
    let event = next(&mut stream);
    assert_eq!(event.id, Some(first.to_string()));
    assert_eq!(event.event_type(), "job.enqueued");
    let data = belay_json::parse(&event.data).expect("json");
    assert_eq!(data["type"], "job.enqueued");
    assert_eq!(data["queue"], "emails");
    // The default `retry:` of two seconds reached the client.
    assert_eq!(stream.reconnect_delay(), Duration::from_secs(2));
}

#[test]
fn last_event_id_wins_over_from() {
    let server = TestServer::start();
    let first = server.engine.events().next_seq();
    for queue in ["a", "b", "c"] {
        enqueue(&server, queue);
    }
    // `from` asks for everything, Last-Event-ID says the first was seen.
    let mut stream = open(&server, &format!("?from={first}"), Some(first));
    assert_eq!(next(&mut stream).id, Some((first + 1).to_string()));
    assert_eq!(next(&mut stream).id, Some((first + 2).to_string()));
}

#[test]
fn filters_select_types_and_queues() {
    let server = TestServer::start();
    let first = server.engine.events().next_seq();
    enqueue(&server, "emails");
    server
        .engine
        .pause_queue(&"emails".parse().unwrap())
        .unwrap();
    enqueue(&server, "sms");
    let mut by_type = open(&server, &format!("?from={first}&types=queue.*"), None);
    assert_eq!(next(&mut by_type).event_type(), "queue.paused");
    let mut by_queue = open(&server, &format!("?from={first}&queue=sms"), None);
    let event = next(&mut by_queue);
    assert_eq!(event.id, Some((first + 2).to_string()));

    let bad = server.get("/v1/events?types=job.succeded", None);
    let error = api_error(&bad);
    assert!(
        error.message.contains(r#"did you mean "job.succeeded""#),
        "{}",
        error.message
    );
    let mut req = Request::new(Method::Get, server.url("/v1/events"));
    req.headers.insert("Last-Event-ID", "yesterday");
    let refused = http().request(req).unwrap();
    assert_eq!(api_error(&refused).code, ErrorCode::InvalidArgument);
}

#[test]
fn idle_streams_send_heartbeat_comments() {
    let options = ServerOptions {
        event_heartbeat: Duration::from_millis(30),
        event_retry: Duration::from_millis(1500),
        ..ServerOptions::default()
    };
    let server = TestServer::with(test_config(), options);
    let req = Request::new(Method::Get, server.url("/v1/events"));
    let mut response = http().stream(req).expect("stream opens");
    assert_eq!(response.status, 200);
    assert_eq!(response.header("content-type"), Some("text/event-stream"));
    let mut seen = Vec::new();
    let mut buf = [0u8; 256];
    let deadline = Instant::now() + Duration::from_secs(5);
    while !seen.ends_with(b":\n\n:\n\n") && Instant::now() < deadline {
        let n = response.body.read(&mut buf).expect("read");
        seen.extend_from_slice(&buf[..n]);
    }
    let text = String::from_utf8(seen).expect("utf-8");
    assert!(text.starts_with("retry: 1500\n\n:\n\n:\n\n"), "{text:?}");
}

#[test]
fn a_server_shutdown_ends_open_streams_promptly() {
    let mut server = TestServer::start();
    let mut stream = open(&server, "", None);
    let handle = server.take_handle();
    let started = Instant::now();
    let stopper = std::thread::spawn(move || handle.shutdown_and_join());
    // The stream ends (cleanly, or with the connection closing).
    let ended = stream.next();
    assert!(!matches!(ended, Some(Ok(_))), "{ended:?}");
    stopper.join().expect("no panic");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_stopped_engine_answers_503_with_retry_after() {
    let server = TestServer::start();
    server.engine.shutdown();
    let response = server.get("/v1/events", None);
    assert_eq!(response.status, 503);
    assert_eq!(response.header("retry-after"), Some("1"));
    assert_eq!(api_error(&response).code, ErrorCode::Unavailable);
}
