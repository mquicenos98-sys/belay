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
