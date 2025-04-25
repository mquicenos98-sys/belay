//! The worker registry: who has been leasing, when they were last heard
//! from, and what they hold.
//!
//! Nothing here is persisted. Workers announce themselves with every lease
//! and heartbeat, so the registry refills within one lease interval of a
//! restart; `active_jobs` is re-seeded from the running jobs' leases when
//! the engine opens, so the numbers are right immediately.

use std::collections::BTreeMap;
use std::time::Duration;

use belay_common::Timestamp;
use belay_proto::api::WorkerInfo;
use belay_proto::{QueueName, WorkerId};

/// A completed attempt, for the per-worker counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Leased,
    Succeeded,
    Failed,
}

#[derive(Debug)]
struct Entry {
    info: WorkerInfo,
    lost: bool,
}

#[derive(Debug, Default)]
pub(crate) struct Workers {
    entries: BTreeMap<WorkerId, Entry>,
}
