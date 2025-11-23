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
