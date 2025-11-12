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
