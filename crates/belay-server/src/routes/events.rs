//! `GET /v1/events?from=&types=&queue=`: the engine's event stream as
//! server-sent events.
//!
//! Each event is sent with `id:` its sequence number, `event:` its type
//! (`job.succeeded`) and `data:` its JSON form. A client resumes by
//! sending the last id it saw as `Last-Event-ID`, which takes precedence
//! over `from` (the first sequence number wanted); without either, the
//! stream starts with the oldest event the server still buffers. Resuming
//! from a point the buffer no longer holds, or reading too slowly to keep
//! up, yields an `event: lagged` frame whose data says how many sequence
//! numbers were skipped, then continues with the oldest buffered event.
//! The lag frame has no id, so a reconnecting client still resumes after
//! the last real event it received.
//!
//! Every stream starts with `retry:` and sends a comment line when idle
//! for [`ServerOptions::event_heartbeat`](crate::ServerOptions::event_heartbeat).
//! A stream ends cleanly (final chunk, connection closed) when the server
//! shuts down or its client goes away; a client should reconnect. The
//! stream never answers `204`: events cannot be switched off, and a
//! request arriving while the engine stops gets `503` with `Retry-After`.
//! Streams are local to the node: each node streams its own events.

use std::io;
use std::time::Duration;

use belay_common::debug;
use belay_core::{Delivery, RecvError, Subscription};
use belay_http::{Request, Response, ShutdownSignal, SseEvent, SseWriter};
use belay_json::json;
use belay_proto::api::{encode_body, ApiError};
use belay_proto::event::{Event, EventFilter};

use crate::app::App;
use crate::observe::HttpMetrics;

/// The shortest heartbeat interval honoured: anything shorter would turn
/// an idle stream into a flood of comments.
const MIN_HEARTBEAT: Duration = Duration::from_millis(10);

/// How a stream is paced.
#[derive(Clone, Copy, Debug)]
struct Pacing {
    heartbeat: Duration,
    retry: Duration,
    poll: Duration,
}

pub(super) fn stream(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    let mut filter = EventFilter::from_pairs(req.query.iter())?;
    if let Some(last) = last_event_id(req)? {
        filter.from = Some(last.saturating_add(1));
    }
    // The endpoint already turns requests away once the engine stops; a
    // bus closed by other means ends streams just the same.
    if app.engine.events().is_closed() {
        return Err(ApiError::unavailable("the event stream is closed"));
    }
    let subscription = app.engine.subscribe(filter);
    let shutdown = req.extensions.get::<ShutdownSignal>().cloned();
    let open = app.metrics.as_ref().map(HttpMetrics::stream_opened);
    let pacing = Pacing {
        heartbeat: app.options.event_heartbeat.max(MIN_HEARTBEAT),
        retry: app.options.event_retry,
        poll: app.options.poll(),
    };
    Ok(Response::sse(move |sse| {
        // Held until the stream ends, which is when the body is dropped.
        let _open = &open;
        pump(sse, &subscription, shutdown.as_ref(), pacing)
    }))
}

/// The sequence number in `Last-Event-ID`, if the client sent one.
fn last_event_id(req: &Request) -> Result<Option<u64>, ApiError> {
    match req.last_event_id().map(str::trim) {
        None | Some("") => Ok(None),
        Some(id) => id.parse::<u64>().map(Some).map_err(|_| {
            let shown: String = id.chars().take(64).collect();
            ApiError::invalid_argument(format!(
                "Last-Event-ID must be an event sequence number, got {shown:?}"
            ))
        }),
    }
}

/// Writes deliveries from `subscription` until the bus closes, the server
/// shuts down or the client leaves.
fn pump(
    sse: &mut SseWriter<'_, '_>,
    subscription: &Subscription,
    shutdown: Option<&ShutdownSignal>,
    pacing: Pacing,
) -> io::Result<()> {
    sse.set_heartbeat_interval(pacing.heartbeat);
    sse.retry(pacing.retry)?;
    let mut delivered: u64 = 0;
    let mut lags: u64 = 0;
    let ended = loop {
        if shutdown.is_some_and(ShutdownSignal::is_triggered) {
            break "server shutting down";
        }
        // Wake at least every poll interval: a shutdown or a vanished
        // client is only noticed between waits.
        let wait = sse.until_heartbeat().min(pacing.poll);
        match subscription.recv_timeout(wait) {
            Ok(Delivery::Event(event)) => {
                sse.send(&frame(&event))?;
                delivered += 1;
            }
            Ok(Delivery::Lagged { missed }) => {
                sse.send(&lagged(missed))?;
                lags += 1;
            }
            Err(RecvError::Timeout) => {
                if !sse.is_open() {
                    break "client left or server shutting down";
                }
                sse.heartbeat_if_due()?;
            }
            Err(RecvError::Closed) => break "event bus closed",
        }
    };
    debug!("event stream ended: {ended}"; delivered, lags);
    Ok(())
}

/// One event as an SSE frame.
fn frame(event: &Event) -> SseEvent {
    let data = String::from_utf8_lossy(&encode_body(event)).into_owned();
    SseEvent::new(data)
        .id(event.seq.to_string())
        .event(event.type_name())
}

/// The frame telling a client it missed `missed` sequence numbers.
fn lagged(missed: u64) -> SseEvent {
    SseEvent::new(json!({"type": "lagged", "missed": missed}).to_string()).event("lagged")
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_common::sync::lock;
    use belay_core::{Config, Engine};
    use belay_http::{BodyWriter, Method, SseParser};
    use belay_proto::JobSpec;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    fn engine(buffer: usize) -> Engine {
        let mut config = Config::dev();
        config.events.buffer = buffer;
        Engine::builder()
            .config(config)
            .background(false)
            .start()
            .expect("engine starts")
    }

    const PACING: Pacing = Pacing {
        heartbeat: Duration::from_secs(60),
        retry: Duration::from_millis(1500),
        poll: Duration::from_millis(5),
    };

    /// A sink the test can read while the stream writes to it.
    #[derive(Clone, Default)]
    struct Wire(Arc<Mutex<Vec<u8>>>);

    impl Write for Wire {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            lock(&self.0).extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn parse(text: &str) -> Vec<SseEvent> {
        let mut parser = SseParser::new();
        let mut events = Vec::new();
        parser
            .feed(text.as_bytes(), |e| events.push(e))
            .expect("well-formed stream");
        events
    }

    /// Streams until `frames` frames have been written, then shuts the
    /// stream down and returns what it wrote.
    fn stream(engine: &Engine, filter: EventFilter, frames: usize) -> (String, Vec<SseEvent>) {
        let subscription = engine.subscribe(filter);
        let shutdown = ShutdownSignal::new();
        let wire = Wire::default();
        let writer = {
            let (shutdown, mut wire) = (shutdown.clone(), wire.clone());
            std::thread::spawn(move || {
                let mut out = BodyWriter::new(&mut wire);
                let mut sse = SseWriter::new(&mut out);
                pump(&mut sse, &subscription, Some(&shutdown), PACING)
            })
        };
        let text = || String::from_utf8(lock(&wire.0).clone()).expect("utf-8");
        let deadline = Instant::now() + Duration::from_secs(5);
        while parse(&text()).len() < frames && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        shutdown.trigger();
        writer.join().expect("no panic").expect("writes succeed");
        let text = text();
        let events = parse(&text);
        (text, events)
    }

    #[test]
    fn events_carry_their_sequence_number_type_and_json() {
        let engine = engine(100);
        let first = engine.events().next_seq();
        let id = engine.enqueue(JobSpec::new("emails", "send")).unwrap().id;
        let (text, events) = stream(&engine, EventFilter::new().from_seq(first), 1);
        assert!(text.starts_with("retry: 1500\n\n"), "{text}");
        assert_eq!(events.len(), 1, "{text}");
        let event = &events[0];
        assert_eq!(event.id.as_deref(), Some(first.to_string().as_str()));
        assert_eq!(event.event_type(), "job.enqueued");
        let data = belay_json::parse(&event.data).expect("json data");
        assert_eq!(data["seq"], first);
        assert_eq!(data["job"], id.to_string().as_str());
    }

    #[test]
    fn filters_apply_to_the_stream() {
        let engine = engine(100);
        let first = engine.events().next_seq();
        engine.enqueue(JobSpec::new("emails", "send")).unwrap();
        engine.enqueue(JobSpec::new("sms", "send")).unwrap();
        let filter = EventFilter::new()
            .from_seq(first)
            .for_queue("sms".parse().expect("queue name"));
        // Wait for a frame the filter lets through after the one it drops.
        engine.enqueue(JobSpec::new("sms", "send")).unwrap();
        let (_, events) = stream(&engine, filter, 2);
        let ids: Vec<Option<String>> = events.iter().map(|e| e.id.clone()).collect();
        assert_eq!(
            ids,
            [Some((first + 1).to_string()), Some((first + 2).to_string())]
        );
    }

    #[test]
    fn a_closed_bus_ends_the_stream() {
        let engine = engine(100);
        let subscription = engine.events().subscribe_live(EventFilter::new());
        engine.events().close();
        let mut wire = Vec::new();
        {
            let mut out = BodyWriter::new(&mut wire);
            let mut sse = SseWriter::new(&mut out);
            pump(&mut sse, &subscription, None, PACING).expect("writes to a vec succeed");
        }
        assert_eq!(String::from_utf8(wire).unwrap(), "retry: 1500\n\n");
    }

    #[test]
    fn idle_streams_send_heartbeats() {
        let engine = engine(100);
        let subscription = engine.events().subscribe_live(EventFilter::new());
        let shutdown = ShutdownSignal::new();
        let wire = Wire::default();
        let writer = {
            let (shutdown, mut wire) = (shutdown.clone(), wire.clone());
            std::thread::spawn(move || {
                let mut out = BodyWriter::new(&mut wire);
                let mut sse = SseWriter::new(&mut out);
                let pacing = Pacing {
                    heartbeat: Duration::from_millis(10),
                    ..PACING
                };
                pump(&mut sse, &subscription, Some(&shutdown), pacing)
            })
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !lock(&wire.0).ends_with(b":\n\n:\n\n") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        shutdown.trigger();
        writer.join().expect("no panic").expect("writes succeed");
        let text = String::from_utf8(lock(&wire.0).clone()).expect("utf-8");
        assert!(text.starts_with("retry: 1500\n\n:\n\n:\n\n"), "{text:?}");
    }

    #[test]
    fn last_event_id_must_be_a_sequence_number() {
        let req =
            |id: &str| Request::new(Method::Get, "/v1/events").with_header("Last-Event-ID", id);
        assert_eq!(last_event_id(&req("41")).unwrap(), Some(41));
        assert_eq!(last_event_id(&req(" ")).unwrap(), None);
        assert_eq!(
            last_event_id(&Request::new(Method::Get, "/")).unwrap(),
            None
        );
        let err = last_event_id(&req("abc")).unwrap_err();
        assert_eq!(
            err.message,
            r#"Last-Event-ID must be an event sequence number, got "abc""#
        );
    }
}
