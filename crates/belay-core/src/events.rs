//! The event bus: numbers every event the engines report, keeps the most
//! recent ones for replay, and fans them out to subscribers.
//!
//! [`EventBus`] implements [`EventSink`], so the queue, the scheduler and
//! the workflow engine report straight into it. Each event gets the next
//! sequence number and the time, goes into a bounded ring buffer, and is
//! offered to every live [`Subscription`] whose filter it matches.
//!
//! # Subscribers that fall behind
//!
//! Engines emit while holding their own locks, so emitting must never
//! wait for a slow reader. Each subscriber therefore has a bounded queue;
//! when it is full the subscriber stops receiving pushes and is marked
//! *behind*. Once it has drained its queue it reads directly from the
//! ring buffer, from the first event it did not get, until it has caught
//! up and goes back to live pushes. Nothing is lost as long as the ring
//! buffer still holds that event. If it does not, the subscriber receives
//! [`Delivery::Lagged`] with the number of sequence numbers it missed and
//! continues from the oldest buffered event. Replay from a starting
//! sequence number (`EventFilter::from`, the SSE `Last-Event-ID`) takes
//! the same path.
//!
//! # Locking
//!
//! The bus lock is taken before any subscriber's lock, never the other
//! way round; a reader waiting for events holds neither.

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

use belay_common::sync::{lock, read, wait_timeout, write};
use belay_common::SharedClock;
use belay_proto::event::{Event, EventFilter, EventKind};
use belay_proto::EventSink;

/// Sizing of an [`EventBus`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BusOptions {
    /// Events kept for replay (at least 1).
    pub capacity: usize,
    /// Events queued per live subscriber (at least 1).
    pub subscriber_queue: usize,
    /// Sequence number of the first event.
    pub first_seq: u64,
}

impl Default for BusOptions {
    fn default() -> BusOptions {
        BusOptions {
            capacity: 10_000,
            subscriber_queue: 1024,
            first_seq: 1,
        }
    }
}

/// What a [`Subscription`] yields.
#[derive(Clone, Debug, PartialEq)]
pub enum Delivery {
    /// The next matching event.
    Event(Event),
    /// Events were dropped from the replay buffer before this subscriber
    /// read them. `missed` counts sequence numbers, so it includes events
    /// the filter would not have delivered. Delivery resumes with the
    /// oldest buffered event.
    Lagged {
        /// Sequence numbers skipped.
        missed: u64,
    },
}

/// Why [`Subscription::recv_timeout`] returned without a delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecvError {
    /// Nothing arrived in time.
    Timeout,
    /// The bus was closed (the engine is shutting down) and everything
    /// queued has been delivered.
    Closed,
}

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RecvError::Timeout => "no event arrived in time",
            RecvError::Closed => "the event bus is closed",
        })
    }
}

impl std::error::Error for RecvError {}

/// Counters describing a bus.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BusStats {
    /// Events published since the bus was created.
    pub published: u64,
    /// Sequence number the next event will get.
    pub next_seq: u64,
    /// Events in the replay buffer.
    pub buffered: usize,
    /// Subscriptions alive.
    pub subscribers: usize,
    /// `Lagged` markers delivered.
    pub lagged: u64,
}

type Observer = Arc<dyn Fn(&Event) + Send + Sync>;

/// A sequence-numbered event stream with replay. Clones share the bus.
///
/// ```
/// use std::time::Duration;
/// use belay_common::{ManualClock, Timestamp};
/// use belay_core::events::{Delivery, EventBus};
/// use belay_proto::event::{EventFilter, EventKind};
/// use belay_proto::{EventSink, QueueName};
///
/// let bus = EventBus::new(ManualClock::new(Timestamp::from_secs(1)).shared());
/// bus.emit(EventKind::QueuePaused { queue: QueueName::from_static("a") });
/// let sub = bus.subscribe(EventFilter::new()); // replays from the oldest event
/// bus.emit(EventKind::QueueResumed { queue: QueueName::from_static("a") });
///
/// let first = sub.recv_timeout(Duration::from_millis(10)).unwrap();
/// let Delivery::Event(first) = first else { panic!("expected an event") };
/// assert_eq!((first.seq, first.type_name()), (1, "queue.paused"));
/// let Ok(Delivery::Event(second)) = sub.recv_timeout(Duration::from_millis(10)) else { panic!() };
/// assert_eq!(second.seq, 2);
/// ```
#[derive(Clone)]
pub struct EventBus {
    inner: Arc<Inner>,
}

struct Inner {
    clock: SharedClock,
    capacity: usize,
    subscriber_queue: usize,
    state: Mutex<State>,
    observers: RwLock<Vec<Observer>>,
    lagged: AtomicU64,
}

struct State {
    next_seq: u64,
    published: u64,
    buffer: VecDeque<Event>,
    subscribers: Vec<Weak<Slot>>,
    closed: bool,
}

impl State {
    fn oldest_seq(&self) -> u64 {
        self.buffer.front().map_or(self.next_seq, |e| e.seq)
    }
}

/// One subscriber's side of the bus.
struct Slot {
    filter: EventFilter,
    queue: Mutex<SlotState>,
    ready: Condvar,
}

struct SlotState {
    items: VecDeque<Event>,
    mode: Mode,
    closed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Receiving pushes from `emit`.
    Live,
    /// Not receiving pushes; the next event to read from the buffer is the
    /// first matching one at or after `cursor`.
    Behind { cursor: u64 },
}

impl fmt::Debug for EventBus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventBus")
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl EventBus {
    /// A bus with default sizing, numbering events from 1.
    #[must_use]
    pub fn new(clock: SharedClock) -> EventBus {
        EventBus::with_options(clock, BusOptions::default())
    }

    /// A bus with explicit sizing.
    #[must_use]
    pub fn with_options(clock: SharedClock, options: BusOptions) -> EventBus {
        let capacity = options.capacity.max(1);
        EventBus {
            inner: Arc::new(Inner {
                clock,
                capacity,
                subscriber_queue: options.subscriber_queue.max(1),
                state: Mutex::new(State {
                    next_seq: options.first_seq,
                    published: 0,
                    buffer: VecDeque::with_capacity(capacity.min(4096)),
                    subscribers: Vec::new(),
                    closed: false,
                }),
                observers: RwLock::new(Vec::new()),
                lagged: AtomicU64::new(0),
            }),
        }
    }

    /// Calls `f` with every event from now on, as it is published and
    /// under the bus lock, so `f` must be quick and must not publish.
    pub fn on_event<F>(&self, f: F)
    where
        F: Fn(&Event) + Send + Sync + 'static,
    {
        write(&self.inner.observers).push(Arc::new(f));
    }

    /// Stamps `kind` with the next sequence number and the current time,
    /// buffers it and hands it to subscribers. Returns the sequence number.
    pub fn publish(&self, kind: EventKind) -> u64 {
        let inner = &*self.inner;
        let mut st = lock(&inner.state);
        let event = Event {
            seq: st.next_seq,
            at: inner.clock.now(),
            kind,
        };
        st.next_seq += 1;
        st.published += 1;
        for observer in read(&inner.observers).iter() {
            observer(&event);
        }
        if st.buffer.len() == inner.capacity {
            st.buffer.pop_front();
        }
        st.subscribers.retain(|w| w.strong_count() > 0);
        for slot in st.subscribers.iter().filter_map(Weak::upgrade) {
            slot.offer(&event, inner.subscriber_queue);
        }
        let seq = event.seq;
        st.buffer.push_back(event);
        seq
    }

    /// Subscribes to events matching `filter`, starting at `filter.from`:
    /// buffered events from there are replayed first. Without `from`,
    /// replay starts at the oldest buffered event; use
    /// [`subscribe_live`](Self::subscribe_live) for new events only. A
    /// `from` beyond the next sequence number starts with the next event.
    #[must_use]
    pub fn subscribe(&self, filter: EventFilter) -> Subscription {
        let mut st = lock(&self.inner.state);
        let next = st.next_seq;
        let cursor = filter.from.unwrap_or(0).max(st.oldest_seq());
        let mode = if cursor >= next {
            Mode::Live
        } else {
            Mode::Behind { cursor }
        };
        let slot = Arc::new(Slot {
            filter: EventFilter {
                from: None,
                ..filter
            },
            queue: Mutex::new(SlotState {
                items: VecDeque::new(),
                mode,
                closed: st.closed,
            }),
            ready: Condvar::new(),
        });
        st.subscribers.push(Arc::downgrade(&slot));
        Subscription {
            bus: Arc::clone(&self.inner),
            slot,
        }
    }

    /// Subscribes to events published from now on.
    #[must_use]
    pub fn subscribe_live(&self, filter: EventFilter) -> Subscription {
        let next = self.next_seq();
        self.subscribe(EventFilter {
            from: Some(next),
            ..filter
        })
    }

    /// The sequence number the next event will get.
    #[must_use]
    pub fn next_seq(&self) -> u64 {
        lock(&self.inner.state).next_seq
    }

    /// A copy of the buffered events at or after `from`, oldest first.
    #[must_use]
    pub fn replay(&self, from: u64) -> Vec<Event> {
        let st = lock(&self.inner.state);
        let skip = from.saturating_sub(st.oldest_seq()) as usize;
        st.buffer.iter().skip(skip).cloned().collect()
    }

    /// Wakes every subscriber: once they have read what is queued for
    /// them, they get [`RecvError::Closed`]. Events published afterwards
    /// are still numbered and buffered, but no subscription sees them.
    pub fn close(&self) {
        let mut st = lock(&self.inner.state);
        st.closed = true;
        for slot in st.subscribers.drain(..).filter_map(|w| w.upgrade()) {
            lock(&slot.queue).closed = true;
            slot.ready.notify_all();
        }
    }

    /// Whether [`close`](Self::close) was called.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        lock(&self.inner.state).closed
    }

    /// Current counters.
    #[must_use]
    pub fn stats(&self) -> BusStats {
        let st = lock(&self.inner.state);
        BusStats {
            published: st.published,
            next_seq: st.next_seq,
            buffered: st.buffer.len(),
            subscribers: st
                .subscribers
                .iter()
                .filter(|w| w.strong_count() > 0)
                .count(),
            lagged: self.inner.lagged.load(Ordering::Relaxed),
        }
    }
}

impl EventSink for EventBus {
    fn emit(&self, kind: EventKind) {
        self.publish(kind);
    }
}

impl Slot {
    /// Queues `event` if the subscriber is live, wants it and has room;
    /// otherwise a full queue switches the subscriber to catching up from
    /// the buffer, starting with this event.
    fn offer(&self, event: &Event, room: usize) {
        if !self.filter.matches_kind(&event.kind) {
            return;
        }
        let mut s = lock(&self.queue);
        if s.mode != Mode::Live {
            return;
        }
        if s.items.len() < room {
            s.items.push_back(event.clone());
            drop(s);
            self.ready.notify_one();
        } else {
            s.mode = Mode::Behind { cursor: event.seq };
        }
    }
}

/// A subscriber's handle, returned by [`EventBus::subscribe`]. Dropping it
/// unsubscribes.
pub struct Subscription {
    bus: Arc<Inner>,
    slot: Arc<Slot>,
}

impl fmt::Debug for Subscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = lock(&self.slot.queue);
        f.debug_struct("Subscription")
            .field("filter", &self.slot.filter)
            .field("queued", &s.items.len())
            .field("mode", &s.mode)
            .finish()
    }
}
