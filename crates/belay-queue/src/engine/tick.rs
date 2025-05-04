//! Time-driven work: [`QueueEngine::tick`].
//!
//! The engine never starts threads of its own; whoever embeds it calls
//! `tick` periodically (belay-core does so a few times per second) with
//! the current time, and tests call it with a manual clock's time. One
//! tick, queue by queue:
//!
//! 1. promotes scheduled and retrying jobs whose `run_at` has come;
//! 2. expires leases that ran out: the attempt fails with a
//!    `lease expired` error (or `timed out` when the job's own timeout is
//!    what ended it), and the job retries, dies or is cancelled like any
//!    failed attempt;
//! 3. deletes finished jobs past their queue's retention, at most
//!    [`EngineOptions::retention_per_tick`](crate::EngineOptions::retention_per_tick)
//!    per tick so that a retention backlog is worked off gradually;
//! 4. releases timed uniqueness claims that ran out;
//! 5. reports workers that went silent.
//!
//! Each step works from the ordered indexes, so a tick with nothing due
//! costs a handful of lookups per queue. Work is written in batches of
//! [`EngineOptions::batch_size`](crate::EngineOptions::batch_size) with the
//! engine lock released in between.

use belay_common::{debug, format_duration, Timestamp};
use belay_proto::event::EventKind;
use belay_proto::{Job, JobError, QueueName};

use super::work::stage_failure;
use super::QueueEngine;
use crate::error::Result;
use crate::txn::{Ctx, Effects};

/// What one [`QueueEngine::tick`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Waiting jobs made ready.
    pub promoted: u64,
    /// Leases that ran out.
    pub expired: u64,
    /// Finished jobs deleted for retention.
    pub deleted: u64,
    /// Timed uniqueness claims released.
    pub claims_released: u64,
    /// Workers newly reported lost.
    pub workers_lost: u64,
    /// Retention stopped at its per-tick limit with more to delete; the
    /// next tick continues.
    pub backlog: bool,
}

impl TickReport {
    /// Whether the tick found nothing to do.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        *self == TickReport::default()
    }
}
