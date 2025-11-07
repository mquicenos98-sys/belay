//! Incremental `text/event-stream` parsing, following the WHATWG HTML
//! "event stream interpretation" algorithm.

use std::time::Duration;

use crate::error::{Error, Result};
use crate::sse::SseEvent;

/// Default cap on a single line or event's data, 1 MiB.
const DEFAULT_MAX_EVENT_BYTES: usize = 1 << 20;

/// Turns stream bytes into [`SseEvent`]s.
///
/// Bytes can be fed in arbitrary pieces; lines may end in CRLF, LF or CR,
/// and a CRLF split across two feeds is handled. The last event id and the
/// server-requested retry delay persist across events, as the spec
/// requires, and survive [`reset`](Self::reset) (used between
/// reconnections).
///
/// ```
/// use belay_http::{SseEvent, SseParser};
///
/// let mut parser = SseParser::new();
/// let mut events = Vec::new();
/// parser.feed(b"id: 1\nevent: ping\ndata: a\n", |e| events.push(e))?;
/// parser.feed(b"data: b\n\n: comment\n\n", |e| events.push(e))?;
/// assert_eq!(events, [SseEvent::new("a\nb").event("ping").id("1")]);
/// assert_eq!(parser.last_event_id(), Some("1"));
/// # Ok::<(), belay_http::Error>(())
/// ```
#[derive(Debug)]
pub struct SseParser {
    line: Vec<u8>,
    data: String,
    event: String,
    last_id: String,
    retry: Option<Duration>,
    /// The previous byte was a CR, so an immediately following LF belongs
    /// to the same line terminator.
    after_cr: bool,
    /// Whether the first line of the current stream has been seen (a BOM
    /// is only stripped there).
    started: bool,
    max_event_bytes: usize,
}

impl Default for SseParser {
    fn default() -> Self {
        SseParser::new()
    }
}

impl SseParser {
    /// A parser with a 1 MiB limit per line and per event.
    pub fn new() -> Self {
        SseParser {
            line: Vec::new(),
            data: String::new(),
            event: String::new(),
            last_id: String::new(),
            retry: None,
            after_cr: false,
            started: false,
            max_event_bytes: DEFAULT_MAX_EVENT_BYTES,
        }
    }

    /// Sets the byte limit for one line and for one event's accumulated
    /// data.
    #[must_use]
    pub fn with_max_event_bytes(mut self, limit: usize) -> Self {
        self.max_event_bytes = limit;
        self
    }

    /// The id of the most recent event that set one.
    pub fn last_event_id(&self) -> Option<&str> {
        (!self.last_id.is_empty()).then_some(self.last_id.as_str())
    }

    /// Seeds the last event id, e.g. from a checkpoint.
    pub fn set_last_event_id(&mut self, id: &str) {
        self.last_id = id.to_owned();
    }

    /// The reconnection delay most recently requested by the server.
    pub fn retry(&self) -> Option<Duration> {
        self.retry
    }

    /// Discards a partially received event and line, keeping the last id
    /// and retry delay. Call this when the connection drops: the spec says
    /// an incomplete event at end of stream is not dispatched.
    pub fn reset(&mut self) {
        self.line.clear();
        self.data.clear();
        self.event.clear();
        self.after_cr = false;
        self.started = false;
    }

    /// Feeds bytes, calling `emit` for every completed event.
    pub fn feed(&mut self, bytes: &[u8], mut emit: impl FnMut(SseEvent)) -> Result<()> {
        for &b in bytes {
            if self.after_cr {
                self.after_cr = false;
                if b == b'\n' {
                    continue;
                }
            }
            match b {
                b'\r' => {
                    self.after_cr = true;
                    self.end_line(&mut emit)?;
                }
                b'\n' => self.end_line(&mut emit)?,
                _ => {
                    if self.line.len() >= self.max_event_bytes {
                        return Err(Error::BodyTooLarge {
                            limit: self.max_event_bytes as u64,
                        });
                    }
                    self.line.push(b);
                }
            }
        }
        Ok(())
    }

    fn end_line(&mut self, emit: &mut impl FnMut(SseEvent)) -> Result<()> {
        let mut line = std::mem::take(&mut self.line);
        if !self.started {
            self.started = true;
            if line.starts_with(b"\xef\xbb\xbf") {
                line.drain(..3);
            }
        }
        let result = self.process_line(&line, emit);
        // Hand the allocation back for the next line.
        line.clear();
        self.line = line;
        result
    }

    fn process_line(&mut self, line: &[u8], emit: &mut impl FnMut(SseEvent)) -> Result<()> {
        if line.is_empty() {
            self.dispatch(emit);
            return Ok(());
        }
        if line[0] == b':' {
            return Ok(());
        }
        let (field, value) = match line.iter().position(|&b| b == b':') {
            Some(colon) => {
                let value = &line[colon + 1..];
                (&line[..colon], value.strip_prefix(b" ").unwrap_or(value))
            }
            None => (line, &[][..]),
        };
        let value = String::from_utf8_lossy(value);
        match field {
            b"event" => self.event = value.into_owned(),
            b"data" => {
                if self.data.len() + value.len() >= self.max_event_bytes {
                    return Err(Error::BodyTooLarge {
                        limit: self.max_event_bytes as u64,
                    });
                }
                self.data.push_str(&value);
                self.data.push('\n');
            }
            b"id" => {
                if !value.contains('\0') {
                    self.last_id = value.into_owned();
                }
            }
            b"retry" => {
                if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
                    if let Ok(ms) = value.parse::<u64>() {
                        self.retry = Some(Duration::from_millis(ms));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn dispatch(&mut self, emit: &mut impl FnMut(SseEvent)) {
        if self.data.is_empty() {
            self.event.clear();
            return;
        }
        if self.data.ends_with('\n') {
            self.data.pop();
        }
        let event = std::mem::take(&mut self.event);
        emit(SseEvent {
            id: self.last_event_id().map(str::to_owned),
            event: (!event.is_empty()).then_some(event),
            data: std::mem::take(&mut self.data),
        });
    }
}
