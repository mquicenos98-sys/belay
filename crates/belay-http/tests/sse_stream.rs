//! Server-Sent Events end to end: the server's `SseWriter` streaming to the
//! client's `EventStream`, reconnection with `Last-Event-ID`, and
//! disconnect detection on the server side.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use belay_http::{
    Client, Drain, Error, Request, Response, Router, SseEvent, SseOptions, StatusCode,
};
use common::{serve, url};

fn fast_options() -> SseOptions {
    SseOptions::default()
        .reconnect_delay(Duration::from_millis(10))
        .read_timeout(Some(Duration::from_secs(5)))
}

fn collect(events: impl Iterator<Item = belay_http::Result<SseEvent>>) -> Vec<SseEvent> {
    events.map(|e| e.expect("no stream errors")).collect()
}

#[test]
fn events_arrive_and_the_stream_resumes_from_the_last_id() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let mut router = Router::new();
    router.get("/v1/events", move |req: &mut Request| {
        let last = req.last_event_id().map(str::to_owned);
        log.lock().expect("test lock").push(last.clone());
        match last.as_deref() {
            None => Response::sse(|sse| {
                for i in 1..=3 {
                    let event = SseEvent::new(format!("payload {i}"))
                        .event("tick")
                        .id(i.to_string());
                    sse.send(&event)?;
                }
                Ok(())
            }),
            Some("3") => {
                Response::sse(|sse| sse.send(&SseEvent::new("line one\nline two").id("4")))
            }
            // Nothing newer: 204 tells an event-stream client to stop.
            Some(_) => Response::no_content(),
        }
    });
    let server = serve(router);
    let events = Client::default()
        .sse_with(&url(&server, "/v1/events"), fast_options())
        .expect("connect");
    let received = collect(events);
    assert_eq!(
        received,
        [
            SseEvent::new("payload 1").event("tick").id("1"),
            SseEvent::new("payload 2").event("tick").id("2"),
            SseEvent::new("payload 3").event("tick").id("3"),
            SseEvent::new("line one\nline two").id("4"),
        ]
    );
    let seen = seen.lock().expect("test lock").clone();
    assert_eq!(seen, [None, Some("3".to_owned()), Some("4".to_owned())]);
}

#[test]
fn resume_point_can_be_given_up_front() {
    let mut router = Router::new();
    router.get("/events", |req: &mut Request| match req.last_event_id() {
        Some("41") => Response::sse(|sse| sse.send(&SseEvent::new("next").id("42"))),
        _ => Response::no_content(),
    });
    let server = serve(router);
    let mut events = Client::default()
        .sse_with(&url(&server, "/events"), fast_options().last_event_id("41"))
        .expect("connect");
    let first = events.next().expect("an event").expect("no error");
    assert_eq!(first.data, "next");
    assert_eq!(events.last_event_id(), Some("42"));
    assert!(events.next().is_none());
    assert!(events.is_closed());
}

#[test]
fn server_notices_when_the_client_goes_away() {
    let (ended_tx, ended_rx) = mpsc::channel::<usize>();
    let ended_tx = Mutex::new(ended_tx);
    let mut router = Router::new();
    router.get("/firehose", move |_: &mut Request| {
        let done = ended_tx.lock().expect("test lock").clone();
        Response::sse(move |sse| {
            let mut sent = 0;
            let result = loop {
                if !sse.is_open() {
                    break Ok(());
                }
                if let Err(e) = sse.data(&sent.to_string()) {
                    break Err(e);
                }
                sent += 1;
                thread::sleep(Duration::from_millis(5));
            };
            let _ = done.send(sent);
            result
        })
    });
    let server = serve(router);
    let mut events = Client::default()
        .sse_with(&url(&server, "/firehose"), fast_options())
        .expect("connect");
    for expected in 0..3 {
        let event = events.next().expect("an event").expect("no error");
        assert_eq!(event.data, expected.to_string());
    }
    drop(events);
    let sent = ended_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("the stream should end once the client disconnects");
    assert!(sent >= 3, "sent {sent}");
    let deadline = Instant::now() + Duration::from_secs(3);
    while server.stats().active_connections > 0 {
        assert!(
            Instant::now() < deadline,
            "connection slot was not released"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn comments_heartbeats_and_retry_are_invisible_to_the_iterator() {
    let mut router = Router::new();
    router.get("/events", |req: &mut Request| {
        if req.last_event_id().is_some() {
            return Response::no_content();
        }
        Response::sse(|sse| {
            sse.retry(Duration::from_millis(15))?;
            sse.heartbeat()?;
            sse.comment("connected")?;
            sse.event("job.succeeded", "{\"id\":1}")?;
            sse.heartbeat()?;
            sse.send(&SseEvent::new("{\"id\":2}").event("job.failed").id("2"))
        })
    });
    let server = serve(router);
    let mut events = Client::default()
        .sse_with(&url(&server, "/events"), fast_options())
        .expect("connect");
    let first = events.next().expect("event").expect("ok");
    assert_eq!(first.event_type(), "job.succeeded");
    assert_eq!(first.id, None);
    let second = events.next().expect("event").expect("ok");
    assert_eq!(second.event_type(), "job.failed");
    assert_eq!(events.reconnect_delay(), Duration::from_millis(15));
    assert!(events.next().is_none());
}

#[test]
fn transient_failures_are_retried_until_the_stream_is_back() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let mut router = Router::new();
    router.get("/events", move |_: &mut Request| {
        match counter.fetch_add(1, Ordering::SeqCst) {
            0 => Response::sse(|sse| sse.send(&SseEvent::new("before").id("1"))),
            1 => Response::error(StatusCode::SERVICE_UNAVAILABLE, "restarting"),
            2 => Response::sse(|sse| sse.send(&SseEvent::new("after").id("2"))),
            _ => Response::no_content(),
        }
    });
    let server = serve(router);
    let events = Client::default()
        .sse_with(&url(&server, "/events"), fast_options())
        .expect("connect");
    let data: Vec<String> = collect(events).into_iter().map(|e| e.data).collect();
    assert_eq!(data, ["before", "after"]);
    assert_eq!(hits.load(Ordering::SeqCst), 4);
}

#[test]
fn reconnects_are_bounded_when_configured() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let mut router = Router::new();
    router.get("/empty", move |_: &mut Request| {
        counter.fetch_add(1, Ordering::SeqCst);
        // A stream that ends at once without events.
        Response::sse(|_| Ok(()))
    });
    let server = serve(router);
    let mut events = Client::default()
        .sse_with(&url(&server, "/empty"), fast_options().max_reconnects(2))
        .expect("connect");
    let err = events
        .next()
        .expect("an item")
        .expect_err("gives up after two attempts");
    assert!(matches!(err, Error::ConnectionClosed), "{err}");
    assert!(events.next().is_none());
    assert_eq!(hits.load(Ordering::SeqCst), 3);
}

#[test]
fn error_statuses_and_wrong_content_types_fail_at_connect() {
    let mut router = Router::new();
    router
        .get("/private", |_: &mut Request| {
            Response::error(StatusCode::UNAUTHORIZED, "missing token")
        })
        .get("/plain", |_: &mut Request| Response::text("not a stream"));
    let server = serve(router);
    let client = Client::default();
    match client.sse(&url(&server, "/private")) {
        Err(Error::Status { status, body }) => {
            assert_eq!(status, StatusCode::UNAUTHORIZED);
            assert_eq!(body, "missing token");
        }
        other => panic!("expected a status error, got {other:?}"),
    }
    let err = client
        .sse(&url(&server, "/plain"))
        .expect_err("wrong content type");
    assert!(matches!(err, Error::UnexpectedContentType { .. }), "{err}");
}

#[test]
fn open_stream_ends_promptly_on_server_shutdown() {
    let mut router = Router::new();
    router.get("/live", |_: &mut Request| {
        Response::sse(|sse| {
            sse.data("hello")?;
            while sse.is_open() {
                sse.heartbeat_if_due()?;
                thread::sleep(Duration::from_millis(5));
            }
            Ok(())
        })
    });
    let server = serve(router);
    let mut events = Client::default()
        .sse_with(&url(&server, "/live"), fast_options())
        .expect("connect");
    assert_eq!(events.next().expect("event").expect("ok").data, "hello");
    let start = Instant::now();
    assert_eq!(server.shutdown_and_join(), Drain::Complete);
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
    events.close();
    assert!(events.next().is_none());
}
