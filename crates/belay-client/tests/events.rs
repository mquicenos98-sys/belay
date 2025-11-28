//! The event subscription: request shape, resumption after disconnects
//! (with `from` and `Last-Event-ID` agreeing), de-duplication, skipping of
//! unknown types, and giving up.

mod common;

use std::thread;
use std::time::{Duration, Instant};

use belay_client::{Error, EventOptions};
use belay_common::BackoffPolicy;
use belay_http::{Request, Response, SseEvent};
use belay_json::ToJson;
use belay_proto::api::{ApiError, ErrorCode};
use belay_proto::event::{Event, EventFilter, EventKind};
use belay_proto::QueueName;
use common::{t0, Fake};

fn event(seq: u64) -> Event {
    Event {
        seq,
        at: t0(),
        kind: EventKind::QueuePaused {
            queue: QueueName::from_static("emails"),
        },
    }
}

/// An SSE frame carrying `event` the way a belay server sends it.
fn frame(event: &Event) -> SseEvent {
    SseEvent::new(event.to_json().to_string())
        .event(event.type_name())
        .id(event.seq.to_string())
}

/// A stream that sends `frames` and then ends.
fn stream_of(frames: Vec<SseEvent>) -> impl FnOnce(&Request) -> Response + Send + 'static {
    move |_| {
        Response::sse(move |sse| {
            for frame in &frames {
                sse.send(frame)?;
            }
            Ok(())
        })
    }
}

fn fast() -> EventOptions {
    EventOptions::default()
        .reconnect_backoff(BackoffPolicy::constant(Duration::from_millis(1)))
        .idle_timeout(Duration::from_secs(5))
}

fn seqs(items: Vec<Result<Event, Error>>) -> Vec<u64> {
    items.into_iter().map(|e| e.expect("event").seq).collect()
}

#[test]
fn resumes_after_disconnects_without_gaps_or_repeats() {
    let fake = Fake::start();
    fake.reply_with(stream_of(vec![
        frame(&event(1)),
        frame(&event(2)),
        frame(&event(3)),
    ]));
    // A server that replays the last event it sent before the drop.
    fake.reply_with(stream_of(vec![frame(&event(3)), frame(&event(4))]));
    fake.reply_with(|_| Response::no_content());
    let client = fake.client();

    let mut events = client
        .events_with(&EventFilter::new(), fast())
        .expect("subscribed");
    let received = seqs(events.by_ref().collect());

    assert_eq!(received, [1, 2, 3, 4]);
    assert_eq!(events.last_seq(), Some(4));
    assert_eq!(events.reconnects(), 1, "the 204 ends the stream");
    assert!(events.is_closed());

    let seen = fake.requests();
    assert_eq!(seen.len(), 3);
    assert!(seen[0].query.is_empty());
    assert_eq!(seen[0].header("last-event-id"), None);
    assert_eq!(seen[1].query_pairs(), [("from", "4")]);
    assert_eq!(seen[1].header("last-event-id"), Some("3"));
    assert_eq!(seen[2].query_pairs(), [("from", "5")]);
    assert_eq!(seen[2].header("last-event-id"), Some("4"));
    for s in &seen {
        assert_eq!(s.call(), "GET /v1/events");
        assert_eq!(s.header("accept"), Some("text/event-stream"));
        assert_eq!(s.header("authorization"), Some("Bearer test-token"));
    }
}

#[test]
fn the_filter_becomes_the_query_and_the_resume_point() {
    let fake = Fake::start();
    fake.reply_with(|_| Response::no_content());
    let filter = EventFilter::new()
        .from_seq(10)
        .with_type("job.*".parse().expect("pattern"))
        .with_type("schedule.fired".parse().expect("pattern"))
        .for_queue(QueueName::from_static("emails"));

    let events = fake.client().events(&filter).expect("subscribed");

    assert_eq!(events.count(), 0);
    let seen = fake.only();
    assert_eq!(
        seen.query_pairs(),
        [
            ("from", "10"),
            ("types", "job.*,schedule.fired"),
            ("queue", "emails")
        ]
    );
    assert_eq!(seen.header("last-event-id"), Some("9"));
}

#[test]
fn starting_from_zero_sends_no_last_event_id() {
    let fake = Fake::start();
    fake.reply_with(|_| Response::no_content());
    let events = fake
        .client()
        .events(&EventFilter::new().from_seq(0))
        .expect("subscribed");
    drop(events);
    let seen = fake.only();
    assert_eq!(seen.query_pairs(), [("from", "0")]);
    assert_eq!(seen.header("last-event-id"), None);
}

#[test]
fn unknown_event_types_are_skipped_and_counted() {
    let fake = Fake::start();
    let from_the_future = SseEvent::new(
        r#"{"seq":2,"at":"2025-03-04T05:06:07.089Z","type":"job.teleported","to":"mars"}"#,
    )
    .id("2");
    fake.reply_with(stream_of(vec![
        frame(&event(1)),
        from_the_future,
        frame(&event(3)),
    ]));
    fake.reply_with(|_| Response::no_content());

    let mut events = fake
        .client()
        .events_with(&EventFilter::new(), fast())
        .expect("subscribed");
    let received = seqs(events.by_ref().collect());

    assert_eq!(received, [1, 3]);
    assert_eq!(events.skipped(), 1);
    assert_eq!(fake.last().query_pairs(), [("from", "4")]);
}

#[test]
fn a_malformed_event_is_an_error_but_the_stream_goes_on() {
    let fake = Fake::start();
    fake.reply_with(stream_of(vec![
        frame(&event(1)),
        SseEvent::new(r#"{"seq":2,"at":"2025-03-04T05:06:07.089Z","type":"job.succeeded"}"#)
            .id("2"),
        SseEvent::new("not json at all").id("3"),
        frame(&event(4)),
    ]));
    fake.reply_with(|_| Response::no_content());

    let items: Vec<_> = fake
        .client()
        .events_with(&EventFilter::new(), fast())
        .expect("subscribed")
        .collect();

    assert_eq!(items.len(), 4);
    assert_eq!(items[0].as_ref().map(|e| e.seq).ok(), Some(1));
    for bad in &items[1..3] {
        let err = bad.as_ref().unwrap_err();
        assert!(matches!(err, Error::Decode(_)), "{err}");
    }
    assert!(items[1]
        .as_ref()
        .unwrap_err()
        .to_string()
        .contains("missing required field"));
    assert_eq!(items[3].as_ref().map(|e| e.seq).ok(), Some(4));
    assert_eq!(fake.last().query_pairs(), [("from", "5")]);
}

#[test]
fn an_error_answer_on_subscribe_is_returned_at_once() {
    let fake = Fake::start();
    fake.reply_error(ApiError::unauthenticated("missing token"));
    let err = fake.client().events(&EventFilter::new()).unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::Unauthenticated));
    assert_eq!(fake.count(), 1);
}

#[test]
fn gives_up_after_max_reconnects_with_the_last_error() {
    let fake = Fake::start();
    fake.reply_with(stream_of(vec![frame(&event(1))]));
    fake.reply_error(ApiError::unavailable("restarting"));
    fake.reply_error(ApiError::unavailable("still restarting"));

    let items: Vec<_> = fake
        .client()
        .events_with(&EventFilter::new(), fast().max_reconnects(2))
        .expect("subscribed")
        .collect();

    assert_eq!(items.len(), 2);
    assert_eq!(items[0].as_ref().map(|e| e.seq).ok(), Some(1));
    let err = items[1].as_ref().unwrap_err();
    assert_eq!(
        err.api_error().map(|e| e.message.as_str()),
        Some("still restarting")
    );
    assert_eq!(fake.count(), 3);
}

#[test]
fn a_fatal_answer_while_reconnecting_ends_the_stream() {
    let fake = Fake::start();
    fake.reply_with(stream_of(vec![frame(&event(1))]));
    fake.reply_error(ApiError::permission_denied("token revoked"));

    let items: Vec<_> = fake
        .client()
        .events_with(&EventFilter::new(), fast())
        .expect("subscribed")
        .collect();

    assert_eq!(items.len(), 2);
    assert_eq!(
        items[1].as_ref().unwrap_err().code(),
        Some(ErrorCode::PermissionDenied)
    );
}

#[test]
fn the_servers_retry_hint_replaces_the_backoff() {
    let fake = Fake::start();
    fake.reply_with(|_| {
        Response::sse(|sse| {
            sse.retry(Duration::from_millis(1))?;
            sse.send(&frame(&event(1)))
        })
    });
    fake.reply_with(|_| Response::no_content());
    let slow =
        EventOptions::default().reconnect_backoff(BackoffPolicy::constant(Duration::from_secs(5)));
    let started = Instant::now();

    let received = seqs(
        fake.client()
            .events_with(&EventFilter::new(), slow)
            .expect("subscribed")
            .collect(),
    );

    assert_eq!(received, [1]);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "reconnected after the server's 1 ms, not the 5 s backoff"
    );
}

#[test]
fn a_silent_connection_is_replaced_after_the_idle_timeout() {
    let fake = Fake::start();
    fake.reply_with(|_| {
        Response::sse(|sse| {
            sse.send(&frame(&event(1)))?;
            // No heartbeat: the client should give up on this connection.
            thread::sleep(Duration::from_millis(300));
            Ok(())
        })
    });
    fake.reply_with(stream_of(vec![frame(&event(2))]));
    fake.reply_with(|_| Response::no_content());
    let options = fast().idle_timeout(Duration::from_millis(100));

    let mut events = fake
        .client()
        .events_with(&EventFilter::new(), options)
        .expect("subscribed");
    let received = seqs(events.by_ref().collect());

    assert_eq!(received, [1, 2]);
    assert_eq!(fake.requests()[1].query_pairs(), [("from", "2")]);
}

#[test]
fn close_ends_iteration() {
    let fake = Fake::start();
    fake.reply_with(stream_of(vec![frame(&event(1)), frame(&event(2))]));
    let mut events = fake
        .client()
        .events_with(&EventFilter::new(), fast())
        .expect("subscribed");

    assert_eq!(events.next().map(|e| e.expect("event").seq), Some(1));
    events.close();

    assert!(events.is_closed());
    assert!(events.next().is_none());
    assert_eq!(fake.count(), 1, "no reconnection after close");
}
