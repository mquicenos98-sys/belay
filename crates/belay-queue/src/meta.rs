//! The compact per-job entry the engine keeps in memory.
//!
//! The store already holds every job document (the log-structured store
//! keeps its whole table in memory), so a second decoded copy of each job
//! would double the footprint of payload-heavy queues. Instead the engine
//! keeps a [`Meta`] per job: exactly the fields its indexes are keyed on,
//! plus the lease token so stale worker calls are rejected without a store
//! read. Documents are decoded when a job changes, which every change
//! must do anyway to write the new version.

use belay_common::Timestamp;
use belay_proto::{Job, JobId, JobState, LeaseToken, QueueName, TenantId, WorkerId};

/// Index fields of one job, derived from its document by [`Meta::of`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Meta {
    pub(crate) queue: QueueName,
    pub(crate) state: JobState,
    pub(crate) priority: i16,
    /// When the job became (or becomes) eligible to run.
    pub(crate) run_at: Timestamp,
    pub(crate) created_at: Timestamp,
    pub(crate) finished_at: Option<Timestamp>,
    pub(crate) tenant: Option<TenantId>,
    pub(crate) lease: Option<LeaseMeta>,
}

/// The parts of a running job's lease the engine checks and indexes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LeaseMeta {
    pub(crate) worker: WorkerId,
    pub(crate) token: LeaseToken,
    pub(crate) expires_at: Timestamp,
}
