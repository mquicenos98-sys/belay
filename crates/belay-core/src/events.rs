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
