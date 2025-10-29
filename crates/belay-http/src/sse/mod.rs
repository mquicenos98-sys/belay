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

impl SseEvent {
    /// An unnamed event carrying `data`.
    pub fn new(data: impl Into<String>) -> Self {
        SseEvent {
            id: None,
            event: None,
            data: data.into(),
        }
    }

    /// Sets the event type (builder style).
    #[must_use]
    pub fn event(mut self, name: impl Into<String>) -> Self {
        self.event = Some(name.into());
        self
    }

    /// Sets the id (builder style).
    #[must_use]
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// The event type, `message` when none was given.
    pub fn event_type(&self) -> &str {
        self.event.as_deref().unwrap_or("message")
    }

    /// Serializes the event, rejecting ids and types that contain line
    /// breaks (they would split into separate fields) and ids with NUL
    /// (which clients ignore).
    pub fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        if let Some(id) = &self.id {
            if id.contains(|c| c == '\r' || c == '\n' || c == '\0') {
                return Err(invalid("event id must not contain line breaks or nul"));
            }
            push_field(out, "id", id);
        }
        if let Some(event) = &self.event {
            if event.contains(|c| c == '\r' || c == '\n') {
                return Err(invalid("event type must not contain line breaks"));
            }
            push_field(out, "event", event);
        }
        for line in split_lines(&self.data) {
            push_field(out, "data", line);
        }
        out.push(b'\n');
        Ok(())
    }
}

impl std::fmt::Display for SseEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = Vec::new();
        self.encode(&mut out).map_err(|_| std::fmt::Error)?;
        f.write_str(&String::from_utf8_lossy(&out))
    }
}

fn invalid(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

fn push_field(out: &mut Vec<u8>, name: &str, value: &str) {
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(b": ");
    out.extend_from_slice(value.as_bytes());
    out.push(b'\n');
}

/// Splits on CRLF, CR or LF, the three line terminators the format allows;
/// always yields at least one (possibly empty) line.
fn split_lines(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = Some(text);
    std::iter::from_fn(move || {
        let current = rest?;
        match current.find(|c| c == '\r' || c == '\n') {
            Some(i) => {
                let skip = if current[i..].starts_with("\r\n") {
                    2
                } else {
                    1
                };
                rest = Some(&current[i + skip..]);
                Some(&current[..i])
            }
            None => {
                rest = None;
                Some(current)
            }
        }
    })
}
