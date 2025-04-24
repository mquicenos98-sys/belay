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

impl LatencyLog {
    pub(crate) fn new(capacity: usize) -> LatencyLog {
        LatencyLog {
            samples: VecDeque::with_capacity(capacity.min(1024)),
            capacity,
            dropped: 0,
            summary: LatencySummary::default(),
        }
    }

    pub(crate) fn observe(&mut self, sample: LeaseLatency) {
        self.summary.observe(sample.waited);
        if self.capacity == 0 {
            self.dropped += 1;
            return;
        }
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
            self.dropped += 1;
        }
        self.samples.push_back(sample);
    }

    pub(crate) fn drain(&mut self) -> Vec<LeaseLatency> {
        self.samples.drain(..).collect()
    }

    pub(crate) fn summary(&self) -> LatencySummary {
        self.summary
    }

    pub(crate) fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// One queue's numbers.
#[derive(Clone, Debug, PartialEq)]
pub struct QueueStats {
    /// The queue.
    pub name: QueueName,
    /// Whether leasing is paused.
    pub paused: bool,
    /// Jobs per state.
    pub counts: StateCounts,
    /// How long the oldest ready job has been waiting.
    pub oldest_ready_age: Option<Duration>,
    /// Activity over the throughput window.
    pub throughput: Throughput,
    /// Uniqueness keys currently claimed.
    pub unique_keys: usize,
    /// Tenants with ready jobs.
    pub tenants: usize,
    /// Rate-limit tokens available now, when the queue is rate limited.
    pub rate_tokens: Option<f64>,
}

/// A snapshot of the whole engine, for metrics exporters.
#[derive(Clone, Debug, PartialEq)]
pub struct EngineStats {
    /// Per-queue numbers, by queue name.
    pub queues: Vec<QueueStats>,
    /// Jobs per state across all queues.
    pub totals: StateCounts,
    /// Event counts since open.
    pub counters: Counters,
    /// Lease latency since open.
    pub lease_latency: LatencySummary,
    /// Latency samples dropped because nobody drained them in time.
    pub latency_samples_dropped: u64,
    /// Workers in the registry.
    pub workers: usize,
    /// Lease requests currently long-polling.
    pub waiting_leases: usize,
}

impl EngineStats {
    /// The stats of queue `name`.
    #[must_use]
    pub fn queue(&self, name: &str) -> Option<&QueueStats> {
        self.queues.iter().find(|q| q.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ms: u64) -> LeaseLatency {
        LeaseLatency {
            queue: QueueName::from_static("q"),
            waited: Duration::from_millis(ms),
        }
    }

    #[test]
    fn latency_log_is_bounded_and_keeps_the_newest() {
        let mut log = LatencyLog::new(2);
        for ms in [10, 20, 30] {
            log.observe(sample(ms));
        }
        assert_eq!(log.dropped(), 1);
        let drained: Vec<u64> = log
            .drain()
            .iter()
            .map(|s| s.waited.as_millis() as u64)
            .collect();
        assert_eq!(drained, [20, 30]);
        assert!(log.drain().is_empty());
        let summary = log.summary();
        assert_eq!(summary.count, 3);
        assert_eq!(summary.max, Duration::from_millis(30));
        assert_eq!(summary.mean(), Duration::from_millis(20));
    }

    #[test]
    fn zero_capacity_only_summarises() {
        let mut log = LatencyLog::new(0);
        log.observe(sample(5));
        assert!(log.drain().is_empty());
        assert_eq!((log.dropped(), log.summary().count), (1, 1));
        assert_eq!(LatencySummary::default().mean(), Duration::ZERO);
    }

    #[test]
    fn counters_add_field_by_field() {
        let mut a = Counters {
            enqueued: 1,
            lease_lost: 2,
            ..Counters::default()
        };
        a += Counters {
            enqueued: 3,
            heartbeats: 4,
            ..Counters::default()
        };
        assert_eq!((a.enqueued, a.heartbeats, a.lease_lost), (4, 4, 2));
    }
}
