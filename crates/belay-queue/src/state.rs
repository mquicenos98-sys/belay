//! The engine's committed in-memory state and how a transaction is applied
//! to it.
//!
//! [`State`] only ever changes in [`State::apply`], which runs after the
//! transaction's batch has been written to the store. Applying cannot fail:
//! everything that could go wrong (missing jobs, refused transitions,
//! uniqueness conflicts, the write itself) has been dealt with while
//! staging, so memory and store move together or not at all.

use std::collections::BTreeMap;
use std::time::Duration;

use belay_common::id::UlidGenerator;
use belay_common::{Rng, Timestamp, Ulid};
use belay_proto::event::EventKind;
use belay_proto::{Job, JobId, JobState, QueueName};

use crate::error::{Error, Result};
use crate::meta::Meta;
use crate::queue::QueueState;
use crate::records::QueueRecord;
use crate::stats::{Counters, LatencyLog};
use crate::txn::{Effects, Txn};
use crate::workers::Workers;

/// Everything behind the engine lock.
#[derive(Debug)]
pub(crate) struct Core {
    pub(crate) st: State,
    pub(crate) gen: Gen,
}

/// Committed indexes and bookkeeping.
#[derive(Debug)]
pub(crate) struct State {
    /// Every job, by id: lookups by id and unfiltered listings.
    pub(crate) jobs: BTreeMap<JobId, Meta>,
    pub(crate) queues: BTreeMap<QueueName, QueueState>,
    pub(crate) workers: Workers,
    pub(crate) counters: Counters,
    pub(crate) latency: LatencyLog,
    /// Where the next round-robin lease request starts.
    pub(crate) round_robin: usize,
    /// Throughput window for queues created from now on.
    window: Duration,
}
