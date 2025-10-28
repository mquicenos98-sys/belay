//! Server-Sent Events (the WHATWG `text/event-stream` format): the event
//! type, a writer for server handlers and an incremental parser used by the
//! client's [`EventStream`](crate::EventStream).

mod parser;

use std::io;
use std::time::{Duration, Instant};

use crate::headers::names::CACHE_CONTROL;
use crate::response::{BodyWriter, Response};

pub use parser::SseParser;

/// One event.
///
/// ```
/// use belay_http::SseEvent;
///
/// let event = SseEvent::new("{\"seq\":42}").event("job.succeeded").id("42");
/// assert_eq!(event.event_type(), "job.succeeded");
/// assert_eq!(event.to_string(), "id: 42\nevent: job.succeeded\ndata: {\"seq\":42}\n\n");
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SseEvent {
    /// The event id; the client sends the last one back as `Last-Event-ID`
    /// when it reconnects.
    pub id: Option<String>,
    /// The event type; `None` means the default type, `message`.
    pub event: Option<String>,
    /// The payload. May contain newlines.
    pub data: String,
}
