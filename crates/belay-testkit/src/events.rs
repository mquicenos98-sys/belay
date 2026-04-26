//! [`EventLog`], which collects a server's event stream for assertions,
//! and [`first_gap`] for checking sequence numbers.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use belay_client::{Client, EventOptions};
use belay_common::sync::{lock, wait_timeout};
use belay_proto::event::{Event, EventFilter};
use belay_proto::JobId;

use crate::error::{Error, Result};

#[derive(Default)]
struct Collected {
    events: Vec<Event>,
    /// Why the stream ended, if it did with an error.
    error: Option<String>,
    ended: bool,
}

#[derive(Default)]
struct Shared {
    collected: Mutex<Collected>,
    changed: Condvar,
}

/// A server's event stream, followed over HTTP on a thread of its own and
/// collected for assertions.
///
/// The log reads `GET /v1/events` through the typed client, which resumes
/// after disconnects, so what it holds is what any API consumer would
/// have seen. It stops at the server's shutdown (after a few reconnection
/// attempts) or when dropped.
///
/// ```
/// use belay_testkit::{fixtures::job, TestServer, DEFAULT_TIMEOUT};
///
/// let server = TestServer::start()?;
/// let events = server.event_log()?;
/// let id = server.client().jobs().enqueue(&job("emails", "send"))?.id;
/// let enqueued = events.wait_for_job_event(id, "job.enqueued", DEFAULT_TIMEOUT)?;
/// assert_eq!(enqueued.kind.job(), Some(id));
/// # Ok::<(), belay_testkit::Error>(())
/// ```
pub struct EventLog {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
}

impl EventLog {
    /// Subscribes with `filter` (its `from`, types and queue) and starts
    /// collecting. The first connection is made before this returns, so a
    /// refused subscription (a token without `read`) fails here.
    pub fn start(client: &Client, filter: &EventFilter) -> Result<EventLog> {
        let options = EventOptions::default()
            .idle_timeout(Duration::from_secs(10))
            .max_reconnects(3);
        let subscription = client.events_with(filter, options)?;
        let shared = Arc::new(Shared::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (collector, stopping) = (Arc::clone(&shared), Arc::clone(&stop));
        thread::Builder::new()
            .name("belay-testkit-events".to_owned())
            .spawn(move || {
                for item in subscription {
                    if stopping.load(Ordering::SeqCst) {
                        break;
                    }
                    let mut collected = lock(&collector.collected);
                    match item {
                        Ok(event) => collected.events.push(event),
                        Err(e) => collected.error = Some(e.to_string()),
                    }
                    drop(collected);
                    collector.changed.notify_all();
                }
                lock(&collector.collected).ended = true;
                collector.changed.notify_all();
            })?;
        Ok(EventLog { shared, stop })
    }

    /// Every event received so far, in sequence order.
    #[must_use]
    pub fn events(&self) -> Vec<Event> {
        lock(&self.shared.collected).events.clone()
    }

    /// Number of events received.
    #[must_use]
    pub fn len(&self) -> usize {
        lock(&self.shared.collected).events.len()
    }

    /// Whether nothing has been received.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The events about job `id`.
    #[must_use]
    pub fn of_job(&self, id: JobId) -> Vec<Event> {
        self.matching(|e| e.kind.job() == Some(id))
    }

    /// The type names of the events about job `id`, e.g. `["job.enqueued",
    /// "job.started", "job.succeeded"]`.
    #[must_use]
    pub fn types_of_job(&self, id: JobId) -> Vec<&'static str> {
        self.of_job(id).iter().map(Event::type_name).collect()
    }

    /// How many events of type `type_name` arrived.
    #[must_use]
    pub fn count(&self, type_name: &str) -> usize {
        let collected = lock(&self.shared.collected);
        collected
            .events
            .iter()
            .filter(|e| e.type_name() == type_name)
            .count()
    }

    /// The events matching `predicate`.
    #[must_use]
    pub fn matching(&self, predicate: impl Fn(&Event) -> bool) -> Vec<Event> {
        let collected = lock(&self.shared.collected);
        collected
            .events
            .iter()
            .filter(|e| predicate(e))
            .cloned()
            .collect()
    }

    /// The error that ended or interrupted the stream, if any.
    #[must_use]
    pub fn error(&self) -> Option<String> {
        lock(&self.shared.collected).error.clone()
    }

    /// Whether the stream has ended.
    #[must_use]
    pub fn has_ended(&self) -> bool {
        lock(&self.shared.collected).ended
    }

    /// Waits until `condition` holds for the events received so far.
    pub fn wait_for(&self, timeout: Duration, condition: impl Fn(&[Event]) -> bool) -> Result<()> {
        let started = Instant::now();
        let mut collected = lock(&self.shared.collected);
        loop {
            if condition(&collected.events) {
                return Ok(());
            }
            let waited = started.elapsed();
            if waited >= timeout || collected.ended {
                let reason = match (&collected.error, collected.ended) {
                    (Some(e), _) => format!(", stream error: {e}"),
                    (None, true) => ", the stream ended".to_owned(),
                    (None, false) => String::new(),
                };
                return Err(Error::timeout(
                    format!(
                        "events matching the condition ({} received{reason})",
                        collected.events.len()
                    ),
                    waited,
                ));
            }
            collected = wait_timeout(&self.shared.changed, collected, timeout - waited).0;
        }
    }

    /// Waits for an event matching `predicate` and returns the first one.
    pub fn wait_for_event(
        &self,
        timeout: Duration,
        predicate: impl Fn(&Event) -> bool,
    ) -> Result<Event> {
        self.wait_for(timeout, |events| events.iter().any(&predicate))?;
        let collected = lock(&self.shared.collected);
        collected
            .events
            .iter()
            .find(|e| predicate(e))
            .cloned()
            .ok_or_else(|| Error::timeout("an event that was seen a moment ago", timeout))
    }

    /// Waits for an event of type `type_name` about job `id`.
    pub fn wait_for_job_event(
        &self,
        id: JobId,
        type_name: &str,
        timeout: Duration,
    ) -> Result<Event> {
        self.wait_for_event(timeout, |e| {
            e.kind.job() == Some(id) && e.type_name() == type_name
        })
        .map_err(|e| match e {
            Error::Timeout { waited, .. } => {
                let seen = self.types_of_job(id).join(", ");
                Error::timeout(
                    format!("{type_name} for job {id} (seen for it: [{seen}])"),
                    waited,
                )
            }
            other => other,
        })
    }
}

impl Drop for EventLog {
    fn drop(&mut self) {
        // The reader is blocked on the stream; it notices at its next
        // event or when the stream ends with the server.
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl fmt::Debug for EventLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let collected = lock(&self.shared.collected);
        f.debug_struct("EventLog")
            .field("events", &collected.events.len())
            .field("ended", &collected.ended)
            .field("error", &collected.error)
            .finish()
    }
}
