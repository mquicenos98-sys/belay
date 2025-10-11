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
