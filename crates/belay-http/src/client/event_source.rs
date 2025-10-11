//! Consuming Server-Sent Events with automatic reconnection, in the manner
//! of the browser `EventSource`.
//!
//! Deviations from the browser behaviour, both deliberate: `502`, `503` and
//! `504` answers are treated as transient and retried (a restarting server
//! should not end a long-running subscription), and the number of
//! consecutive reconnection attempts can be bounded.

use std::collections::VecDeque;
use std::fmt;
use std::io::Read;
use std::time::Duration;

use crate::client::{Client, RequestOptions, ResponseBody};
use crate::error::{Error, Result};
use crate::headers::names::{ACCEPT, CACHE_CONTROL, CONTENT_TYPE, LAST_EVENT_ID};
use crate::headers::Headers;
use crate::method::Method;
use crate::request::Request;
use crate::sse::{SseEvent, SseParser};
use crate::status::StatusCode;
use crate::url::Url;

/// Options for [`Client::sse_with`].
#[derive(Clone, Debug)]
pub struct SseOptions {
    /// Extra request headers (e.g. `Authorization`), sent on every
    /// connection attempt.
    pub headers: Headers,
    /// Resume after this event id: sent as `Last-Event-ID` on the first
    /// connection. Later reconnections send the id of the last event
    /// received.
    pub last_event_id: Option<String>,
    /// Wait before reconnecting, until the server overrides it with a
    /// `retry:` field. Default 3 s.
    pub reconnect_delay: Duration,
    /// Consecutive reconnection attempts without receiving an event before
    /// the stream gives up with the last error; `None` (the default) keeps
    /// trying forever.
    pub max_reconnects: Option<u32>,
    /// Longest silence tolerated on an open stream before it is considered
    /// dead and reconnected. Keep it comfortably above the server's
    /// heartbeat interval. Default 60 s.
    pub read_timeout: Option<Duration>,
    /// Limit on one event's size. Default 1 MiB.
    pub max_event_bytes: usize,
}

impl Default for SseOptions {
    fn default() -> Self {
        SseOptions {
            headers: Headers::new(),
            last_event_id: None,
            reconnect_delay: Duration::from_secs(3),
            max_reconnects: None,
            read_timeout: Some(Duration::from_secs(60)),
            max_event_bytes: 1 << 20,
        }
    }
}

impl SseOptions {
    /// Adds a request header (builder style).
    #[must_use]
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.append(name, value);
        self
    }

    /// Sets [`last_event_id`](Self::last_event_id).
    #[must_use]
    pub fn last_event_id(mut self, id: impl Into<String>) -> Self {
        self.last_event_id = Some(id.into());
        self
    }

    /// Sets [`reconnect_delay`](Self::reconnect_delay).
    #[must_use]
    pub fn reconnect_delay(mut self, delay: Duration) -> Self {
        self.reconnect_delay = delay;
        self
    }

    /// Sets [`max_reconnects`](Self::max_reconnects).
    #[must_use]
    pub fn max_reconnects(mut self, attempts: u32) -> Self {
        self.max_reconnects = Some(attempts);
        self
    }

    /// Sets [`read_timeout`](Self::read_timeout).
    #[must_use]
    pub fn read_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.read_timeout = timeout;
        self
    }
}

enum Opened {
    Stream(ResponseBody),
    NoContent,
}

/// An event-stream subscription; iterate to receive events.
///
/// Blocking: `next` waits for the next event, reconnecting (with
/// `Last-Event-ID`) whenever the connection drops. Iteration ends with
/// `None` when the server answers `204 No Content` (the standard way to
/// tell a client to stop), after a fatal error (yielded once as `Err`), or
/// after [`close`](Self::close).
///
/// ```no_run
/// use belay_http::{Client, SseOptions};
///
/// let client = Client::default();
/// let events = client.sse_with(
///     "http://127.0.0.1:7070/v1/events?types=job.failed",
///     SseOptions::default().header("Authorization", "Bearer s3cret"),
/// )?;
/// for event in events {
///     let event = event?;
///     println!("{} {}: {}", event.id.as_deref().unwrap_or("-"), event.event_type(), event.data);
/// }
/// # Ok::<(), belay_http::Error>(())
/// ```
pub struct EventStream {
    client: Client,
    url: Url,
    options: SseOptions,
    parser: SseParser,
    body: Option<ResponseBody>,
    pending: VecDeque<SseEvent>,
    delay: Duration,
    failures: u32,
    last_error: Option<Error>,
    closed: bool,
    buf: Vec<u8>,
}

impl EventStream {
    pub(crate) fn connect(client: Client, url: Url, options: SseOptions) -> Result<EventStream> {
        let parser = SseParser::new().with_max_event_bytes(options.max_event_bytes);
        let mut stream = EventStream {
            client,
            url,
            delay: options.reconnect_delay,
            options,
            parser,
            body: None,
            pending: VecDeque::new(),
            failures: 0,
            last_error: None,
            closed: false,
            buf: vec![0; 8 * 1024],
        };
        match stream.open()? {
            Opened::Stream(body) => stream.body = Some(body),
            Opened::NoContent => stream.closed = true,
        }
        Ok(stream)
    }

    /// The id of the last event received (or the one resumed from).
    pub fn last_event_id(&self) -> Option<&str> {
        self.parser.last_event_id()
    }

    /// The stream's URL.
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// The current reconnection delay (possibly set by the server).
    pub fn reconnect_delay(&self) -> Duration {
        self.delay
    }

    /// Closes the connection; iteration then ends.
    pub fn close(&mut self) {
        self.closed = true;
        self.body = None;
    }

    /// Whether the stream has ended.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    fn open(&mut self) -> Result<Opened> {
        let mut req = Request::new(Method::Get, self.url.to_string());
        req.headers = self.options.headers.clone();
        req.headers.insert(ACCEPT, "text/event-stream");
        req.headers.insert(CACHE_CONTROL, "no-cache");
        if let Some(id) = self.parser.last_event_id() {
            req.headers.insert(LAST_EVENT_ID, id);
        }
        let options = RequestOptions {
            read_timeout: self.options.read_timeout,
            redirects: None,
        };
        let mut response = self.client.stream_with(req, &options)?;
        match response.status {
            StatusCode::NO_CONTENT => return Ok(Opened::NoContent),
            StatusCode::OK => {}
            status => {
                let body = response.body.read_to_vec().unwrap_or_default();
                let end = body.len().min(512);
                return Err(Error::Status {
                    status,
                    body: String::from_utf8_lossy(&body[..end]).trim_end().to_owned(),
                });
            }
        }
        let content_type = response.header(CONTENT_TYPE).unwrap_or("");
        let media_type = content_type.split(';').next().unwrap_or("").trim();
        if !media_type.eq_ignore_ascii_case("text/event-stream") {
            return Err(Error::UnexpectedContentType {
                expected: "text/event-stream",
                found: content_type.to_owned(),
            });
        }
        self.parser.reset();
        Ok(Opened::Stream(response.body))
    }

    /// Replaces the current connection, after the reconnection delay.
    /// Returns an item to yield when the stream has ended.
    fn reconnect(&mut self) -> Option<Option<Result<SseEvent>>> {
        if let Some(max) = self.options.max_reconnects {
            if self.failures >= max {
                self.closed = true;
                let error = self.last_error.take().unwrap_or(Error::ConnectionClosed);
                return Some(Some(Err(error)));
            }
        }
        self.failures += 1;
        std::thread::sleep(self.delay);
        match self.open() {
            Ok(Opened::Stream(body)) => {
                self.body = Some(body);
                None
            }
            Ok(Opened::NoContent) => {
                self.closed = true;
                Some(None)
            }
            Err(error) if is_fatal(&error) => {
                self.closed = true;
                Some(Some(Err(error)))
            }
            Err(error) => {
                self.last_error = Some(error);
                None
            }
        }
    }
}

fn is_fatal(error: &Error) -> bool {
    match error {
        Error::Status { status, .. } => !matches!(status.as_u16(), 502..=504),
        Error::UnexpectedContentType { .. }
        | Error::Url(_)
        | Error::UnsupportedScheme(_)
        | Error::InvalidHeader(_)
        | Error::InvalidMethod(_)
        | Error::BodyTooLarge { .. } => true,
        _ => false,
    }
}

impl Iterator for EventStream {
    type Item = Result<SseEvent>;

    fn next(&mut self) -> Option<Result<SseEvent>> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                self.failures = 0;
                self.last_error = None;
                return Some(Ok(event));
            }
            if self.closed {
                return None;
            }
            let read = match self.body.as_mut() {
                Some(body) => body.read(&mut self.buf),
                None => {
                    if let Some(item) = self.reconnect() {
                        return item;
                    }
                    continue;
                }
            };
            match read {
                Ok(n) if n > 0 => {
                    let pending = &mut self.pending;
                    if let Err(error) = self.parser.feed(&self.buf[..n], |e| pending.push_back(e)) {
                        self.close();
                        return Some(Err(error));
                    }
                    if let Some(retry) = self.parser.retry() {
                        self.delay = retry;
                    }
                }
                // End of stream, a read timeout (no heartbeat in time) or a
                // reset: all mean reconnect. A partial event is discarded,
                // as the spec requires.
                Ok(_) | Err(_) => {
                    self.body = None;
                    self.parser.reset();
                }
            }
        }
    }
}

impl fmt::Debug for EventStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventStream")
            .field("url", &self.url)
            .field("last_event_id", &self.last_event_id())
            .field("connected", &self.body.is_some())
            .field("closed", &self.closed)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_statuses_are_retried() {
        let status = |code| Error::Status {
            status: StatusCode::from_u16(code).expect("valid"),
            body: String::new(),
        };
        assert!(!is_fatal(&status(503)));
        assert!(!is_fatal(&status(502)));
        assert!(is_fatal(&status(401)));
        assert!(is_fatal(&status(500)));
        assert!(!is_fatal(&Error::ConnectionClosed));
        assert!(!is_fatal(&Error::Timeout("read")));
    }

    #[test]
    fn option_builders() {
        let options = SseOptions::default()
            .header("Authorization", "Bearer t")
            .last_event_id("41")
            .reconnect_delay(Duration::from_millis(5))
            .max_reconnects(2)
            .read_timeout(None);
        assert_eq!(options.headers.get("authorization"), Some("Bearer t"));
        assert_eq!(options.last_event_id.as_deref(), Some("41"));
        assert_eq!(options.max_reconnects, Some(2));
        assert_eq!(options.read_timeout, None);
    }
}
