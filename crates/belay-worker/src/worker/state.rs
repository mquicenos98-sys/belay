//! State shared by the worker's threads.
//!
//! One mutex guards the table of owned jobs, the queues of jobs waiting to
//! start, the busy-slot count and the per-kind counts, so the capacity
//! arithmetic always sees a consistent picture. Two condition variables
//! hang off it: `work` wakes executor threads, `changed` wakes everyone
//! else (the poller waiting for a free slot, the watchdog waiting for the
//! next deadline, a shutdown waiting for the table to empty).
//!
//! Each job's own life is a small atomic state machine ([`Phase`]) so that
//! exactly one party reports it: the executor when the handler returns,
//! or whoever abandons it first (the heartbeat thread on a lost lease, the
//! watchdog on a timeout past the grace period, the shutdown sequence).
//! Source calls are never made while holding the lock.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use belay_common::sync::{lock, Shutdown};
use belay_proto::source::JobSource;
use belay_proto::{Job, JobId, LeaseToken};

use crate::context::JobSignals;
use crate::registry::{Entry, Registry};
use crate::stats::{Counters, WorkerStats};
use crate::worker::config::WorkerConfig;

/// Where a leased job is in its life on this worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Phase {
    /// Leased; waiting for a slot or for its kind's limit.
    Waiting = 0,
    /// The handler is running.
    Running = 1,
    /// The handler returned; its executor is reporting the result.
    Finishing = 2,
    /// Given up on; whoever abandoned it reports (or cannot, for a lost
    /// lease), and a late handler result is dropped.
    Abandoned = 3,
}

impl Phase {
    fn from_u8(v: u8) -> Phase {
        match v {
            0 => Phase::Waiting,
            1 => Phase::Running,
            2 => Phase::Finishing,
            _ => Phase::Abandoned,
        }
    }
}

/// The time budget of a running attempt.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timing {
    pub(crate) timeout: Option<Duration>,
    pub(crate) deadline: Option<Instant>,
}

/// A job this worker holds a lease on.
pub(crate) struct Tracked {
    pub(crate) job: Arc<Job>,
    pub(crate) token: LeaseToken,
    pub(crate) entry: Arc<Entry>,
    pub(crate) signals: Arc<JobSignals>,
    phase: AtomicU8,
    timing: Mutex<Option<Timing>>,
    /// Set by the watchdog when the deadline passes, so the timeout is
    /// counted and logged once.
    pub(crate) deadline_hit: AtomicBool,
}
