//! Numbers for metrics and dashboards: [`EngineStats`] snapshots,
//! monotonic [`Counters`], and lease latency samples.
//!
//! Counters restart at zero when the engine is opened; exporters that need
//! monotonic series across restarts should treat a decrease as a reset, as
//! Prometheus does. Per-state job counts are not counters: they are exact
//! at all times and survive restarts, because they are rebuilt from the
//! stored jobs.

use std::collections::VecDeque;
use std::ops::AddAssign;
use std::time::Duration;

use belay_proto::{QueueName, StateCounts, Throughput};

/// Monotonic event counts since the engine was opened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Jobs created by enqueue.
    pub enqueued: u64,
    /// Enqueues answered with an existing job (uniqueness).
    pub duplicates: u64,
    /// Enqueues that replaced a pending holder of their uniqueness key.
    pub replaced: u64,
    /// Jobs handed to workers.
    pub leased: u64,
    /// Jobs completed successfully.
    pub succeeded: u64,
    /// Attempts that failed, including expired leases.
    pub failed: u64,
    /// Jobs that died (attempts exhausted or non-retryable).
    pub dead: u64,
    /// Jobs cancelled.
    pub cancelled: u64,
    /// Leases that ran out.
    pub expired: u64,
    /// Dead or cancelled jobs sent back by `retry`.
    pub retried: u64,
    /// Waiting jobs made ready early by `promote`.
    pub promoted: u64,
    /// Jobs deleted: by `delete`, `purge`, `delete_queue` or retention.
    pub deleted: u64,
    /// Lease requests received (a long poll counts once).
    pub lease_requests: u64,
    /// Lease requests that returned no job.
    pub empty_leases: u64,
    /// Heartbeats accepted.
    pub heartbeats: u64,
    /// Worker calls rejected with `lease_lost`.
    pub lease_lost: u64,
}

impl AddAssign for Counters {
    fn add_assign(&mut self, rhs: Counters) {
        self.enqueued += rhs.enqueued;
        self.duplicates += rhs.duplicates;
        self.replaced += rhs.replaced;
        self.leased += rhs.leased;
        self.succeeded += rhs.succeeded;
        self.failed += rhs.failed;
        self.dead += rhs.dead;
        self.cancelled += rhs.cancelled;
        self.expired += rhs.expired;
        self.retried += rhs.retried;
        self.promoted += rhs.promoted;
        self.deleted += rhs.deleted;
        self.lease_requests += rhs.lease_requests;
        self.empty_leases += rhs.empty_leases;
        self.heartbeats += rhs.heartbeats;
        self.lease_lost += rhs.lease_lost;
    }
}

/// How long a job waited between becoming eligible and being leased.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseLatency {
    /// The job's queue.
    pub queue: QueueName,
    /// Time from eligibility (`run_at`) to lease.
    pub waited: Duration,
}

/// Aggregate of every lease latency since open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LatencySummary {
    /// Leases measured.
    pub count: u64,
    /// Sum of the waits.
    pub total: Duration,
    /// Longest wait.
    pub max: Duration,
}

impl LatencySummary {
    /// Mean wait, zero when nothing was measured.
    #[must_use]
    pub fn mean(&self) -> Duration {
        match u32::try_from(self.count) {
            Ok(0) => Duration::ZERO,
            Ok(n) => self.total / n,
            Err(_) => Duration::from_secs_f64(self.total.as_secs_f64() / self.count as f64),
        }
    }

    fn observe(&mut self, waited: Duration) {
        self.count += 1;
        self.total = self.total.saturating_add(waited);
        self.max = self.max.max(waited);
    }
}

/// Recent latency samples for exporters, plus the running summary.
///
/// Samples are kept in a bounded buffer that a metrics exporter drains
/// periodically and feeds into its histogram; when nobody drains it the
/// oldest samples are dropped (and counted), so memory stays bounded.
#[derive(Debug)]
pub(crate) struct LatencyLog {
    samples: VecDeque<LeaseLatency>,
    capacity: usize,
    dropped: u64,
    summary: LatencySummary,
}
