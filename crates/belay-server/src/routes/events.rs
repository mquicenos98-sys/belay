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
        filter.from = filter.from.or(Some(last.saturating_add(1)));
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
