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

impl Subscription {
    /// The next delivery, waiting up to `timeout` for one.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Delivery, RecvError> {
        let deadline = Instant::now().checked_add(timeout);
        loop {
            {
                let mut s = lock(&self.slot.queue);
                loop {
                    if let Some(event) = s.items.pop_front() {
                        return Ok(Delivery::Event(event));
                    }
                    if s.closed {
                        return Err(RecvError::Closed);
                    }
                    if s.mode != Mode::Live {
                        break;
                    }
                    let remaining = match deadline {
                        Some(d) => d.saturating_duration_since(Instant::now()),
                        None => Duration::from_secs(3600),
                    };
                    if remaining.is_zero() {
                        return Err(RecvError::Timeout);
                    }
                    s = wait_timeout(&self.slot.ready, s, remaining).0;
                }
            }
            if let Some(delivery) = self.catch_up() {
                return Ok(delivery);
            }
        }
    }

    /// The next delivery if one is available now.
    pub fn try_recv(&self) -> Result<Delivery, RecvError> {
        self.recv_timeout(Duration::ZERO)
    }

    /// The next delivery, waiting as long as it takes; `None` once the bus
    /// is closed.
    pub fn recv(&self) -> Option<Delivery> {
        loop {
            match self.recv_timeout(Duration::from_secs(3600)) {
                Ok(delivery) => return Some(delivery),
                Err(RecvError::Closed) => return None,
                Err(RecvError::Timeout) => {}
            }
        }
    }

    /// Takes one step of catching up from the buffer: a lag marker, the
    /// next matching buffered event, or (having reached the end) a switch
    /// back to live pushes, in which case `None`.
    fn catch_up(&self) -> Option<Delivery> {
        let st = lock(&self.bus.state);
        let mut s = lock(&self.slot.queue);
        let Mode::Behind { cursor } = s.mode else {
            return None;
        };
        if !s.items.is_empty() || s.closed {
            // Queued events precede the cursor; the caller delivers them
            // (or reports the close) first.
            return None;
        }
        let oldest = st.oldest_seq();
        if cursor < oldest {
            s.mode = Mode::Behind { cursor: oldest };
            self.bus.lagged.fetch_add(1, Ordering::Relaxed);
            return Some(Delivery::Lagged {
                missed: oldest - cursor,
            });
        }
        let start = (cursor - oldest) as usize;
        let found = st
            .buffer
            .range(start.min(st.buffer.len())..)
            .find(|e| self.slot.filter.matches_kind(&e.kind));
        match found {
            Some(event) => {
                s.mode = Mode::Behind {
                    cursor: event.seq + 1,
                };
                Some(Delivery::Event(event.clone()))
            }
            None => {
                // Everything published so far has been seen; the bus lock
                // is held, so no event can slip in before pushes resume.
                s.mode = Mode::Live;
                None
            }
        }
    }
}

impl Iterator for Subscription {
    type Item = Delivery;

    /// Blocks for the next delivery; ends when the bus closes.
    fn next(&mut self) -> Option<Delivery> {
        self.recv()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_common::{ManualClock, Timestamp};
    use belay_proto::event::EventPattern;
    use belay_proto::QueueName;
    use std::thread;

    fn bus(capacity: usize, subscriber_queue: usize) -> EventBus {
        EventBus::with_options(
            ManualClock::new(Timestamp::from_secs(1_000)).shared(),
            BusOptions {
                capacity,
                subscriber_queue,
                first_seq: 1,
            },
        )
    }

    fn paused(queue: &'static str) -> EventKind {
        EventKind::QueuePaused {
            queue: QueueName::from_static(queue),
        }
    }

    fn resumed(queue: &'static str) -> EventKind {
        EventKind::QueueResumed {
            queue: QueueName::from_static(queue),
        }
    }

    fn next_seq(sub: &Subscription) -> Result<u64, Delivery> {
        match sub.try_recv() {
            Ok(Delivery::Event(e)) => Ok(e.seq),
            Ok(other) => Err(other),
            Err(e) => panic!("expected a delivery, got {e}"),
        }
    }

    #[test]
    fn events_are_numbered_stamped_and_buffered() {
        let bus = bus(3, 8);
        for q in ["a", "b", "c", "d"] {
            bus.emit(paused(q));
        }
        let buffered = bus.replay(0);
        let seqs: Vec<u64> = buffered.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, [2, 3, 4]);
        assert_eq!(buffered[0].at, Timestamp::from_secs(1_000));
        assert_eq!(bus.replay(4).len(), 1);
        let stats = bus.stats();
        assert_eq!((stats.published, stats.next_seq, stats.buffered), (4, 5, 3));
    }

    #[test]
    fn subscribe_replays_from_a_sequence_number_then_goes_live() {
        let bus = bus(100, 8);
        for q in ["a", "b", "c"] {
            bus.emit(paused(q));
        }
        let sub = bus.subscribe(EventFilter::new().from_seq(2));
        assert_eq!(next_seq(&sub), Ok(2));
        bus.emit(paused("d"));
        assert_eq!(next_seq(&sub), Ok(3));
        assert_eq!(next_seq(&sub), Ok(4));
        assert_eq!(sub.try_recv(), Err(RecvError::Timeout));
        bus.emit(paused("e"));
        assert_eq!(next_seq(&sub), Ok(5));
    }

    #[test]
    fn live_subscriptions_skip_the_past_and_filters_apply() {
        let bus = bus(100, 8);
        bus.emit(paused("a"));
        let live =
            bus.subscribe_live(EventFilter::new().with_type(EventPattern::Type("queue.resumed")));
        let queue_b = bus.subscribe(EventFilter::new().for_queue(QueueName::from_static("b")));
        bus.emit(paused("b"));
        bus.emit(resumed("a"));
        assert_eq!(next_seq(&live), Ok(3));
        assert_eq!(live.try_recv(), Err(RecvError::Timeout));
        assert_eq!(next_seq(&queue_b), Ok(2));
        assert_eq!(queue_b.try_recv(), Err(RecvError::Timeout));
    }

    #[test]
    fn a_full_queue_catches_up_from_the_buffer_without_loss() {
        let bus = bus(100, 2);
        let sub = bus.subscribe_live(EventFilter::new());
        for q in ["a", "b", "c", "d", "e"] {
            bus.emit(paused(q));
        }
        let seqs: Vec<u64> = (0..5).map(|_| next_seq(&sub).unwrap()).collect();
        assert_eq!(seqs, [1, 2, 3, 4, 5]);
        assert_eq!(sub.try_recv(), Err(RecvError::Timeout));
        assert_eq!(bus.stats().lagged, 0);
        bus.emit(paused("f"));
        assert_eq!(next_seq(&sub), Ok(6));
    }

    #[test]
    fn a_subscriber_behind_the_buffer_gets_a_lag_marker_and_resyncs() {
        let bus = bus(3, 1);
        let sub = bus.subscribe_live(EventFilter::new());
        for i in 0..10 {
            bus.emit(if i % 2 == 0 {
                paused("a")
            } else {
                resumed("a")
            });
        }
        // Event 1 was queued; 2 overflowed the queue and was evicted from
        // the buffer (which holds 8, 9, 10) along with 3..7.
        assert_eq!(next_seq(&sub), Ok(1));
        assert_eq!(next_seq(&sub), Err(Delivery::Lagged { missed: 6 }));
        let rest: Vec<u64> = (0..3).map(|_| next_seq(&sub).unwrap()).collect();
        assert_eq!(rest, [8, 9, 10]);
        assert_eq!(bus.stats().lagged, 1);
    }

    #[test]
    fn close_wakes_waiters_after_queued_events_are_delivered() {
        let bus = bus(10, 10);
        let sub = bus.subscribe_live(EventFilter::new());
        bus.emit(paused("a"));
        let waiter = {
            let bus = bus.clone();
            thread::spawn(move || {
                let sub = bus.subscribe_live(EventFilter::new());
                sub.recv_timeout(Duration::from_secs(10))
            })
        };
        // Give the waiter a moment to block, then close.
        while bus.stats().subscribers < 2 {
            thread::yield_now();
        }
        bus.close();
        assert_eq!(waiter.join().unwrap(), Err(RecvError::Closed));
        assert_eq!(next_seq(&sub), Ok(1));
        assert_eq!(sub.try_recv(), Err(RecvError::Closed));
        assert!(bus.subscribe(EventFilter::new()).try_recv().is_err());
    }

    #[test]
    fn dropped_subscriptions_are_forgotten() {
        let bus = bus(10, 10);
        let sub = bus.subscribe(EventFilter::new());
        assert_eq!(bus.stats().subscribers, 1);
        drop(sub);
        assert_eq!(bus.stats().subscribers, 0);
        bus.emit(paused("a"));
        assert_eq!(bus.stats().published, 1);
    }

    #[test]
    fn observers_see_every_event_in_order() {
        let bus = bus(10, 10);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        bus.on_event(move |e| lock(&sink).push(e.seq));
        bus.emit(paused("a"));
        bus.emit(paused("b"));
        assert_eq!(*lock(&seen), [1, 2]);
    }

    #[test]
    fn concurrent_publishers_and_subscribers_see_every_event_in_order() {
        const PUBLISHERS: usize = 4;
        const EACH: usize = 500;
        let bus = bus(PUBLISHERS * EACH, 16);
        let subscribers: Vec<_> = (0..3)
            .map(|_| {
                let sub = bus.subscribe_live(EventFilter::new());
                thread::spawn(move || {
                    let mut seqs = Vec::new();
                    while seqs.len() < PUBLISHERS * EACH {
                        match sub.recv_timeout(Duration::from_secs(10)) {
                            Ok(Delivery::Event(e)) => seqs.push(e.seq),
                            Ok(Delivery::Lagged { missed }) => panic!("lagged by {missed}"),
                            Err(e) => panic!("{e}"),
                        }
                    }
                    seqs
                })
            })
            .collect();
        let publishers: Vec<_> = (0..PUBLISHERS)
            .map(|_| {
                let bus = bus.clone();
                thread::spawn(move || {
                    for _ in 0..EACH {
                        bus.emit(paused("q"));
                    }
                })
            })
            .collect();
        for p in publishers {
            p.join().unwrap();
        }
        let expected: Vec<u64> = (1..=(PUBLISHERS * EACH) as u64).collect();
        for s in subscribers {
            assert_eq!(s.join().unwrap(), expected);
        }
    }
}
