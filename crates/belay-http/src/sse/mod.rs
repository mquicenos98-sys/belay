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

/// Default interval between keep-alive comments.
pub const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(15);

/// Writes an event stream from a server handler.
///
/// Each method writes one complete field block and flushes it, so events
/// reach the client immediately. Use [`Response::sse`] to get one:
///
/// ```
/// use std::sync::mpsc;
/// use belay_http::{Response, SseEvent};
///
/// let (tx, rx) = mpsc::channel::<String>();
/// let response = Response::sse(move |sse| {
///     sse.retry(std::time::Duration::from_secs(2))?;
///     while sse.is_open() {
///         match rx.recv_timeout(sse.until_heartbeat()) {
///             Ok(json) => sse.send(&SseEvent::new(json).event("job.succeeded"))?,
///             Err(mpsc::RecvTimeoutError::Timeout) => sse.heartbeat_if_due()?,
///             Err(mpsc::RecvTimeoutError::Disconnected) => break,
///         }
///     }
///     Ok(())
/// });
/// tx.send("{}".into()).unwrap();
/// drop(tx);
/// let body = response.body.into_bytes()?;
/// assert_eq!(body, b"retry: 2000\n\nevent: job.succeeded\ndata: {}\n\n");
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Debug)]
pub struct SseWriter<'w, 'a> {
    out: &'w mut BodyWriter<'a>,
    scratch: Vec<u8>,
    heartbeat: Duration,
    last_write: Instant,
}

impl<'w, 'a> SseWriter<'w, 'a> {
    /// Wraps a body writer.
    pub fn new(out: &'w mut BodyWriter<'a>) -> Self {
        SseWriter {
            out,
            scratch: Vec::new(),
            heartbeat: DEFAULT_HEARTBEAT,
            last_write: Instant::now(),
        }
    }

    /// Changes the keep-alive interval used by
    /// [`heartbeat_if_due`](Self::heartbeat_if_due).
    pub fn set_heartbeat_interval(&mut self, interval: Duration) {
        self.heartbeat = interval;
    }

    /// Sends an event.
    pub fn send(&mut self, event: &SseEvent) -> io::Result<()> {
        self.scratch.clear();
        event.encode(&mut self.scratch)?;
        self.flush_scratch()
    }

    /// Sends an unnamed event with `data`.
    pub fn data(&mut self, data: &str) -> io::Result<()> {
        self.send(&SseEvent::new(data))
    }

    /// Sends a named event with `data`.
    pub fn event(&mut self, name: &str, data: &str) -> io::Result<()> {
        self.send(&SseEvent::new(data).event(name))
    }

    /// Sends a comment, which clients ignore; each line is prefixed with
    /// `:`.
    pub fn comment(&mut self, text: &str) -> io::Result<()> {
        self.scratch.clear();
        for line in split_lines(text) {
            self.scratch.push(b':');
            if !line.is_empty() {
                self.scratch.push(b' ');
                self.scratch.extend_from_slice(line.as_bytes());
            }
            self.scratch.push(b'\n');
        }
        self.scratch.push(b'\n');
        self.flush_scratch()
    }

    /// Tells the client how long to wait before reconnecting.
    pub fn retry(&mut self, delay: Duration) -> io::Result<()> {
        self.scratch.clear();
        self.scratch
            .extend_from_slice(format!("retry: {}\n\n", delay.as_millis()).as_bytes());
        self.flush_scratch()
    }

    /// Sends a keep-alive comment. Besides keeping proxies from timing the
    /// connection out, the write is how a vanished client gets detected.
    pub fn heartbeat(&mut self) -> io::Result<()> {
        self.comment("")
    }

    /// Sends a heartbeat if nothing has been written for the heartbeat
    /// interval.
    pub fn heartbeat_if_due(&mut self) -> io::Result<()> {
        if self.last_write.elapsed() >= self.heartbeat {
            self.heartbeat()?;
        }
        Ok(())
    }

    /// Time left until the next heartbeat is due: a good timeout for
    /// blocking on the event source.
    pub fn until_heartbeat(&self) -> Duration {
        self.heartbeat.saturating_sub(self.last_write.elapsed())
    }

    /// `false` once the client has disconnected or the server is shutting
    /// down; the stream should then return.
    pub fn is_open(&self) -> bool {
        !self.out.is_shutting_down() && self.out.is_client_connected()
    }

    fn flush_scratch(&mut self) -> io::Result<()> {
        self.out.send(&self.scratch)?;
        self.last_write = Instant::now();
        Ok(())
    }
}
