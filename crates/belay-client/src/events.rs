//! Following the server's event stream (`GET /v1/events`, server-sent
//! events), resuming where it left off after every disconnect.
//!
//! Each connection asks for the events after the last one delivered, in
//! both the ways a server may read it: the `from` query parameter and the
//! `Last-Event-ID` header. Events are delivered in sequence order without
//! repeats, even from a server that replays a few on reconnect.

use std::fmt;
use std::time::Duration;

use belay_common::{BackoffPolicy, Rng};
use belay_http::header::AUTHORIZATION;
use belay_http::{EventStream, SseEvent, SseOptions};
use belay_json::{FromJson, Value};
use belay_proto::api::decode_error;
use belay_proto::event::{Event, EventFilter, EventKind};

use crate::client::Client;
use crate::error::{DecodeError, Error, Result};

const EVENTS_PATH: &str = "/v1/events";

/// Settings for [`Client::events_with`].
#[derive(Clone, Debug, PartialEq)]
pub struct EventOptions {
    /// Longest silence tolerated on an open stream before it is presumed
    /// dead and replaced. belay servers send a heartbeat every 15 s, so
    /// keep this well above that. Default 60 s.
    pub idle_timeout: Duration,
    /// Delay between reconnection attempts; `None` uses the client's
    /// [`backoff`](crate::ClientConfig::backoff). A `retry:` field from the
    /// server takes precedence.
    pub reconnect_backoff: Option<BackoffPolicy>,
    /// Consecutive reconnection attempts without receiving an event before
    /// the subscription gives up with the last error; `None` (the default)
    /// keeps trying for as long as the iterator is polled.
    pub max_reconnects: Option<u32>,
}

impl Default for EventOptions {
    fn default() -> EventOptions {
        EventOptions {
            idle_timeout: Duration::from_secs(60),
            reconnect_backoff: None,
            max_reconnects: None,
        }
    }
}

impl EventOptions {
    /// Sets [`idle_timeout`](Self::idle_timeout).
    #[must_use]
    pub fn idle_timeout(mut self, timeout: Duration) -> EventOptions {
        self.idle_timeout = timeout;
        self
    }

    /// Sets [`reconnect_backoff`](Self::reconnect_backoff).
    #[must_use]
    pub fn reconnect_backoff(mut self, backoff: BackoffPolicy) -> EventOptions {
        self.reconnect_backoff = Some(backoff);
        self
    }

    /// Sets [`max_reconnects`](Self::max_reconnects).
    #[must_use]
    pub fn max_reconnects(mut self, attempts: u32) -> EventOptions {
        self.max_reconnects = Some(attempts);
        self
    }
}

impl Client {
    /// Subscribes to events matching `filter` (`GET /v1/events`); see
    /// [`events_with`](Self::events_with).
    pub fn events(&self, filter: &EventFilter) -> Result<Subscription> {
        self.events_with(filter, EventOptions::default())
    }

    /// Subscribes to events matching `filter`: its types and queue select
    /// events, and `from` is the first sequence number wanted (`None`
    /// starts at the oldest event the server still holds).
    ///
    /// The first connection is made before this returns, so a refused
    /// connection or an error answer (`401`, `400` for a bad filter) is
    /// reported here. Afterwards the subscription reconnects on its own.
    ///
    /// ```no_run
    /// use belay_client::Client;
    /// use belay_proto::event::EventFilter;
    ///
    /// let client = Client::from_env()?;
    /// let filter = EventFilter::new().with_type("job.dead".parse().expect("known type"));
    /// for event in client.events(&filter)? {
    ///     let event = event?;
    ///     println!("#{} {} {:?}", event.seq, event.type_name(), event.kind.job());
    /// }
    /// # Ok::<(), belay_client::Error>(())
    /// ```
    pub fn events_with(&self, filter: &EventFilter, options: EventOptions) -> Result<Subscription> {
        let backoff = options.reconnect_backoff.unwrap_or(self.config().backoff);
        let mut subscription = Subscription {
            client: self.clone(),
            filter: filter.clone(),
            options,
            backoff,
            stream: None,
            last_seq: None,
            server_retry: None,
            failures: 0,
            reconnects: 0,
            skipped: 0,
            last_error: None,
            finished: false,
            rng: Rng::from_entropy(),
        };
        let from = subscription.resume_from();
        subscription.stream = Some(subscription.open(from)?);
        Ok(subscription)
    }
}

/// A live subscription to the event stream; iterate to receive events.
///
/// Blocking: `next` waits for the next event, reconnecting with backoff
/// whenever the connection drops. Iteration ends when the server answers
/// `204 No Content` (its way of saying "stop"), after an error that
/// reconnecting cannot fix (yielded once), when
/// [`max_reconnects`](EventOptions::max_reconnects) runs out, or after
/// [`close`](Self::close).
///
/// An event whose type this client does not know (from a newer server) is
/// skipped and counted in [`skipped`](Self::skipped); an event of a known
/// type that fails to decode is yielded as a [`Decode`](Error::Decode)
/// error and the stream carries on.
pub struct Subscription {
    client: Client,
    /// Types and queue; `from` is replaced on every connection.
    filter: EventFilter,
    options: EventOptions,
    backoff: BackoffPolicy,
    stream: Option<EventStream>,
    last_seq: Option<u64>,
    /// Reconnection delay requested by the server's `retry:` field.
    server_retry: Option<Duration>,
    /// Reconnection attempts since the last delivered event.
    failures: u32,
    reconnects: u64,
    skipped: u64,
    last_error: Option<Error>,
    finished: bool,
    rng: Rng,
}

enum Decoded {
    Event(Event),
    Skip,
    Invalid(Error),
}

impl Subscription {
    /// Sequence number of the last event delivered (or skipped as
    /// unknown); the stream resumes after it.
    #[must_use]
    pub fn last_seq(&self) -> Option<u64> {
        self.last_seq
    }

    /// Successful reconnections so far.
    #[must_use]
    pub fn reconnects(&self) -> u64 {
        self.reconnects
    }

    /// Events skipped because their type is unknown to this client.
    #[must_use]
    pub fn skipped(&self) -> u64 {
        self.skipped
    }

    /// Closes the connection; iteration then ends.
    pub fn close(&mut self) {
        self.finished = true;
        self.stream = None;
    }

    /// Whether the subscription has ended.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.finished
    }

    fn resume_from(&self) -> Option<u64> {
        match self.last_seq {
            Some(seq) => Some(seq.saturating_add(1)),
            None => self.filter.from,
        }
    }

    /// Opens one connection that starts at `from`. belay-http's own
    /// reconnection is switched off so that each connection carries an
    /// up-to-date `from` as well as `Last-Event-ID`.
    fn open(&self, from: Option<u64>) -> Result<EventStream> {
        let filter = EventFilter {
            from,
            ..self.filter.clone()
        };
        let url = self.client.url(EVENTS_PATH, &filter.to_query());
        let mut options = SseOptions::default()
            .reconnect_delay(Duration::ZERO)
            .max_reconnects(0)
            .read_timeout(Some(self.options.idle_timeout));
        if let Some(auth) = self.client.auth_header() {
            options = options.header(AUTHORIZATION, auth);
        }
        if let Some(previous) = from.and_then(|seq| seq.checked_sub(1)) {
            options = options.last_event_id(previous.to_string());
        }
        self.client
            .http()
            .sse_with(&url, options)
            .map_err(stream_error)
    }

    /// Replaces a dropped connection, waiting between attempts.
    fn reconnect(&mut self) -> Result<()> {
        loop {
            if let Some(max) = self.options.max_reconnects {
                if self.failures >= max {
                    return Err(self
                        .last_error
                        .take()
                        .unwrap_or(Error::Transport(belay_http::Error::ConnectionClosed)));
                }
            }
            self.failures += 1;
            let delay = self
                .server_retry
                .unwrap_or_else(|| self.backoff.delay_for(self.failures, &mut self.rng));
            std::thread::sleep(delay);
            match self.open(self.resume_from()) {
                Ok(stream) => {
                    // A 204 opens an already finished stream: the server
                    // wants no more, which is an ending, not a reconnection.
                    if !stream.is_closed() {
                        self.reconnects += 1;
                    }
                    self.stream = Some(stream);
                    return Ok(());
                }
                Err(e) if is_transient(&e) => self.last_error = Some(e),
                Err(e) => return Err(e),
            }
        }
    }

    /// Decodes one frame. Every frame consumed, good or bad, moves the
    /// resume point to its sequence number (from the JSON, or else the SSE
    /// id), so a bad event is reported once rather than on every
    /// reconnection.
    fn decode(&mut self, sse: &SseEvent) -> Decoded {
        let parsed = belay_json::parse(&sse.data);
        let seq = parsed
            .as_ref()
            .ok()
            .and_then(|v| v.get("seq"))
            .and_then(Value::as_u64)
            .or_else(|| sse.id.as_deref().and_then(|id| id.parse().ok()));
        if let Some(seq) = seq {
            if self.last_seq.is_some_and(|last| seq <= last) {
                // Replayed by the server after a reconnect.
                return Decoded::Skip;
            }
            self.last_seq = Some(seq);
        }
        let value = match parsed {
            Ok(value) => value,
            Err(e) => {
                return Decoded::Invalid(bad_event(format!("event data is not JSON: {e}")));
            }
        };
        let known = value
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| EventKind::TYPES.contains(&t));
        if !known {
            self.skipped += 1;
            return Decoded::Skip;
        }
        match Event::from_json(&value) {
            Ok(event) => Decoded::Event(event),
            Err(e) => Decoded::Invalid(bad_event(e.to_string())),
        }
    }
}

impl Iterator for Subscription {
    type Item = Result<Event>;

    fn next(&mut self) -> Option<Result<Event>> {
        loop {
            if self.finished {
                return None;
            }
            let Some(stream) = self.stream.as_mut() else {
                if let Err(e) = self.reconnect() {
                    self.finished = true;
                    return Some(Err(e));
                }
                continue;
            };
            let item = stream.next();
            let hinted = stream.reconnect_delay();
            if !hinted.is_zero() {
                self.server_retry = Some(hinted);
            }
            match item {
                Some(Ok(sse)) => match self.decode(&sse) {
                    Decoded::Event(event) => {
                        self.failures = 0;
                        self.last_error = None;
                        return Some(Ok(event));
                    }
                    Decoded::Skip => {}
                    Decoded::Invalid(e) => return Some(Err(e)),
                },
                Some(Err(e)) => {
                    let e = stream_error(e);
                    if !is_transient(&e) {
                        self.finished = true;
                        return Some(Err(e));
                    }
                    self.stream = None;
                    self.last_error = Some(e);
                }
                // The server answered 204: nothing more for this client.
                None => self.finished = true,
            }
        }
    }
}

impl fmt::Debug for Subscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Subscription")
            .field("filter", &self.filter)
            .field("last_seq", &self.last_seq)
            .field("connected", &self.stream.is_some())
            .field("reconnects", &self.reconnects)
            .field("skipped", &self.skipped)
            .field("finished", &self.finished)
            .finish()
    }
}

fn bad_event(message: String) -> Error {
    Error::Decode(DecodeError::new(format!("GET {EVENTS_PATH}"), message))
}

/// Error answers become [`Error::Api`] like everywhere else.
fn stream_error(e: belay_http::Error) -> Error {
    match e {
        belay_http::Error::Status { status, body } => {
            Error::Api(decode_error(status.as_u16(), body.as_bytes()))
        }
        other => Error::Transport(other),
    }
}

/// Whether reconnecting may help: a dropped or refused connection, or a
/// server (or proxy) that is temporarily unavailable.
fn is_transient(e: &Error) -> bool {
    match e {
        Error::Transport(e) => e.is_transient(),
        Error::Api(e) => e.is_retryable(),
        Error::Decode(_) | Error::Config(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_http::StatusCode;
    use belay_proto::api::ErrorCode;

    #[test]
    fn error_answers_decode_into_api_errors() {
        let e = stream_error(belay_http::Error::Status {
            status: StatusCode::UNAUTHORIZED,
            body: r#"{"error":{"code":"unauthenticated","message":"missing token"}}"#.into(),
        });
        assert_eq!(e.code(), Some(ErrorCode::Unauthenticated));
        assert!(!is_transient(&e));
        let e = stream_error(belay_http::Error::Status {
            status: StatusCode::BAD_GATEWAY,
            body: String::new(),
        });
        assert_eq!(e.code(), Some(ErrorCode::Unavailable));
        assert!(is_transient(&e));
    }

    #[test]
    fn dropped_connections_are_transient_but_protocol_errors_are_not() {
        assert!(is_transient(&Error::Transport(
            belay_http::Error::ConnectionClosed
        )));
        assert!(!is_transient(&Error::Transport(
            belay_http::Error::UnexpectedContentType {
                expected: "text/event-stream",
                found: "text/html".into(),
            }
        )));
        assert!(!is_transient(&Error::Transport(
            belay_http::Error::BodyTooLarge { limit: 1 }
        )));
    }

    #[test]
    fn options_builders() {
        let options = EventOptions::default()
            .idle_timeout(Duration::from_secs(5))
            .reconnect_backoff(BackoffPolicy::constant(Duration::from_millis(1)))
            .max_reconnects(2);
        assert_eq!(options.idle_timeout, Duration::from_secs(5));
        assert_eq!(options.max_reconnects, Some(2));
        assert!(options.reconnect_backoff.is_some());
    }
}
