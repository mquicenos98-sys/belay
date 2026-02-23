//! The engine's metric families.
//!
//! Three sources feed the registry:
//!
//! - **Events.** Counters by queue (jobs enqueued, started, succeeded,
//!   failed, dead, cancelled, expired) and by schedule (fires, misfires)
//!   are counted from the event bus as events are published, so they
//!   agree exactly with the event stream.
//! - **Lease latencies.** The queue engine records how long each job
//!   waited between becoming eligible and being leased; the housekeeping
//!   duty drains those samples into `belay_lease_wait_seconds`.
//! - **Scrape-time state.** Queue depths, store figures, bus and cluster
//!   numbers are read when the registry is rendered, by a collector that
//!   holds only a weak reference to the engine.
//!
//! | family | type | labels |
//! |---|---|---|
//! | `belay_jobs_enqueued_total` | counter | `queue` |
//! | `belay_jobs_started_total` | counter | `queue` |
//! | `belay_jobs_succeeded_total` | counter | `queue` |
//! | `belay_jobs_failed_total` | counter | `queue` (failed attempts that will be retried) |
//! | `belay_jobs_dead_total` | counter | `queue` |
//! | `belay_jobs_cancelled_total` | counter | `queue` |
//! | `belay_jobs_expired_total` | counter | `queue` (leases that ran out) |
//! | `belay_schedule_fires_total` | counter | `schedule` |
//! | `belay_schedule_misfires_total` | counter | `schedule` |
//! | `belay_workflows_finished_total` | counter | `state` |
//! | `belay_lease_wait_seconds` | histogram | `queue` |
//! | `belay_queue_jobs` | gauge | `queue`, `state` |
//! | `belay_queue_paused` | gauge | `queue` |
//! | `belay_queue_oldest_ready_seconds` | gauge | `queue` |
//! | `belay_workers`, `belay_lease_waiting` | gauge | |
//! | `belay_schedules`, `belay_workflows_running` | gauge | |
//! | `belay_store_keys`, `belay_store_live_bytes`, `belay_store_wal_bytes`, `belay_store_wal_segments`, `belay_store_snapshot_bytes`, `belay_store_last_seq` | gauge | |
//! | `belay_store_checkpoints_total`, `belay_store_wal_syncs_total` | counter | |
//! | `belay_events_published_total`, `belay_event_lags_total` | counter | |
//! | `belay_event_subscribers`, `belay_events_buffered` | gauge | |
//! | `belay_cluster_members` | gauge | `state` |
//! | `belay_uptime_seconds` | gauge | |
//! | `belay_info` | gauge (always 1) | `version`, `node` |
//!
//! With `metrics.process` the `process_*` families of
//! [`ProcessCollector`] are added.

use std::sync::{Arc, Weak};

use belay_metrics::{
    exponential_buckets, Collector, CounterVec, Desc, FamilySnapshot, HistogramVec, MetricType,
    MetricValue, ProcessCollector, Registry,
};
use belay_proto::event::{Event, EventKind};
use belay_proto::JobState;
use belay_queue::LeaseLatency;

use crate::engine::Shared;
use crate::events::EventBus;

/// Every family an engine with `metrics.enabled` registers, besides the
/// `process_*` ones. Vec families appear in a scrape once they have a
/// series; the others always do.
pub const FAMILIES: &[&str] = &[
    "belay_jobs_enqueued_total",
    "belay_jobs_started_total",
    "belay_jobs_succeeded_total",
    "belay_jobs_failed_total",
    "belay_jobs_dead_total",
    "belay_jobs_cancelled_total",
    "belay_jobs_expired_total",
    "belay_schedule_fires_total",
    "belay_schedule_misfires_total",
    "belay_workflows_finished_total",
    "belay_lease_wait_seconds",
    "belay_queue_jobs",
    "belay_queue_paused",
    "belay_queue_oldest_ready_seconds",
    "belay_workers",
    "belay_lease_waiting",
    "belay_schedules",
    "belay_workflows_running",
    "belay_store_keys",
    "belay_store_live_bytes",
    "belay_store_wal_bytes",
    "belay_store_wal_segments",
    "belay_store_snapshot_bytes",
    "belay_store_last_seq",
    "belay_store_checkpoints_total",
    "belay_store_wal_syncs_total",
    "belay_events_published_total",
    "belay_event_lags_total",
    "belay_event_subscribers",
    "belay_events_buffered",
    "belay_cluster_members",
    "belay_uptime_seconds",
    "belay_info",
];

/// Counters fed from the event bus.
#[derive(Clone)]
struct EventCounters {
    enqueued: CounterVec,
    started: CounterVec,
    succeeded: CounterVec,
    failed: CounterVec,
    dead: CounterVec,
    cancelled: CounterVec,
    expired: CounterVec,
    fires: CounterVec,
    misfires: CounterVec,
    workflows: CounterVec,
}

impl EventCounters {
    fn register(r: &Registry) -> belay_metrics::Result<EventCounters> {
        let by_queue = |name: &str, help: &str| r.counter_vec(name, help, &["queue"]);
        Ok(EventCounters {
            enqueued: by_queue("belay_jobs_enqueued_total", "Jobs accepted by the queue.")?,
            started: by_queue("belay_jobs_started_total", "Attempts started by a lease.")?,
            succeeded: by_queue("belay_jobs_succeeded_total", "Jobs completed successfully.")?,
            failed: by_queue(
                "belay_jobs_failed_total",
                "Failed attempts that will be retried.",
            )?,
            dead: by_queue("belay_jobs_dead_total", "Jobs that failed for good.")?,
            cancelled: by_queue("belay_jobs_cancelled_total", "Jobs cancelled.")?,
            expired: by_queue("belay_jobs_expired_total", "Leases that ran out.")?,
            fires: r.counter_vec(
                "belay_schedule_fires_total",
                "Jobs enqueued by schedules.",
                &["schedule"],
            )?,
            misfires: r.counter_vec(
                "belay_schedule_misfires_total",
                "Times schedules found fires missed while belay was down or late.",
                &["schedule"],
            )?,
            workflows: r.counter_vec(
                "belay_workflows_finished_total",
                "Workflow runs finished, by outcome.",
                &["state"],
            )?,
        })
    }

    fn observe(&self, event: &Event) {
        use EventKind::*;
        let (family, label) = match &event.kind {
            JobEnqueued { queue, .. } => (&self.enqueued, queue.as_str()),
            JobStarted { queue, .. } => (&self.started, queue.as_str()),
            JobSucceeded { queue, .. } => (&self.succeeded, queue.as_str()),
            JobFailed { queue, .. } => (&self.failed, queue.as_str()),
            JobDead { queue, .. } => (&self.dead, queue.as_str()),
            JobCancelled { queue, .. } => (&self.cancelled, queue.as_str()),
            JobExpired { queue, .. } => (&self.expired, queue.as_str()),
            ScheduleFired { schedule, .. } => (&self.fires, schedule.as_str()),
            ScheduleMisfired { schedule, .. } => (&self.misfires, schedule.as_str()),
            WorkflowFinished { state, .. } => {
                self.workflows.with_label_values(&[state.as_str()]).inc();
                return;
            }
            _ => return,
        };
        family.with_label_values(&[label]).inc();
    }
}
