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

/// The engine's handle on its registry.
pub(crate) struct Metrics {
    registry: Registry,
    lease_wait: Option<HistogramVec>,
}

impl Metrics {
    /// Registers the event counters, the lease latency histogram and, if
    /// asked, the process collector. With `enabled` false nothing is
    /// registered and the registry stays empty.
    pub(crate) fn register(
        registry: Registry,
        enabled: bool,
        process: bool,
        bus: &EventBus,
    ) -> belay_metrics::Result<Metrics> {
        if !enabled {
            return Ok(Metrics {
                registry,
                lease_wait: None,
            });
        }
        let counters = EventCounters::register(&registry)?;
        bus.on_event(move |event| counters.observe(event));
        // 5ms to about 5.5h: a lease wait is anything from an idle
        // worker's immediate pickup to a backlog drained overnight.
        let buckets = exponential_buckets(0.005, 4.0, 12)?;
        let lease_wait = registry.histogram_vec(
            "belay_lease_wait_seconds",
            "Time jobs waited between becoming eligible and being leased.",
            &["queue"],
            &buckets,
        )?;
        if process {
            registry.register_collector(Arc::new(ProcessCollector::new()))?;
        }
        Ok(Metrics {
            registry,
            lease_wait: Some(lease_wait),
        })
    }

    /// Adds the scrape-time collector. It holds `shared` weakly: a registry
    /// that outlives the engine simply stops reporting its state.
    pub(crate) fn attach(&self, shared: &Arc<Shared>) -> belay_metrics::Result<()> {
        if self.lease_wait.is_none() {
            return Ok(());
        }
        self.registry
            .register_collector(Arc::new(EngineCollector::new(Arc::downgrade(shared))?))
    }

    pub(crate) fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Feeds lease latency samples into the histogram.
    pub(crate) fn observe_lease_waits(&self, samples: &[LeaseLatency]) {
        let Some(histogram) = &self.lease_wait else {
            return;
        };
        for sample in samples {
            histogram
                .with_label_values(&[sample.queue.as_str()])
                .observe_duration(sample.waited);
        }
    }
}

struct Descs {
    queue_jobs: Desc,
    queue_paused: Desc,
    oldest_ready: Desc,
    workers: Desc,
    lease_waiting: Desc,
    schedules: Desc,
    workflows_running: Desc,
    store_keys: Desc,
    store_live_bytes: Desc,
    store_wal_bytes: Desc,
    store_wal_segments: Desc,
    store_snapshot_bytes: Desc,
    store_last_seq: Desc,
    store_checkpoints: Desc,
    store_wal_syncs: Desc,
    events_published: Desc,
    event_lags: Desc,
    event_subscribers: Desc,
    events_buffered: Desc,
    cluster_members: Desc,
    uptime: Desc,
    info: Desc,
}

impl Descs {
    fn new() -> belay_metrics::Result<Descs> {
        let gauge = |name: &str, help: &str, labels: &[&str]| {
            Desc::new(name, help, MetricType::Gauge, labels)
        };
        let counter = |name: &str, help: &str| Desc::new(name, help, MetricType::Counter, &[]);
        Ok(Descs {
            queue_jobs: gauge(
                "belay_queue_jobs",
                "Jobs by queue and state.",
                &["queue", "state"],
            )?,
            queue_paused: gauge(
                "belay_queue_paused",
                "1 if the queue is paused.",
                &["queue"],
            )?,
            oldest_ready: gauge(
                "belay_queue_oldest_ready_seconds",
                "How long the oldest ready job has been waiting.",
                &["queue"],
            )?,
            workers: gauge("belay_workers", "Workers seen recently.", &[])?,
            lease_waiting: gauge(
                "belay_lease_waiting",
                "Lease requests currently long-polling.",
                &[],
            )?,
            schedules: gauge("belay_schedules", "Schedules defined.", &[])?,
            workflows_running: gauge("belay_workflows_running", "Workflow runs in progress.", &[])?,
            store_keys: gauge("belay_store_keys", "Keys in the store.", &[])?,
            store_live_bytes: gauge(
                "belay_store_live_bytes",
                "Bytes of keys and values in the store.",
                &[],
            )?,
            store_wal_bytes: gauge(
                "belay_store_wal_bytes",
                "Bytes of write-ahead log on disk.",
                &[],
            )?,
            store_wal_segments: gauge(
                "belay_store_wal_segments",
                "Write-ahead log segment files.",
                &[],
            )?,
            store_snapshot_bytes: gauge(
                "belay_store_snapshot_bytes",
                "Size of the newest snapshot.",
                &[],
            )?,
            store_last_seq: gauge(
                "belay_store_last_seq",
                "Sequence number of the last write.",
                &[],
            )?,
            store_checkpoints: counter("belay_store_checkpoints_total", "Checkpoints written.")?,
            store_wal_syncs: counter("belay_store_wal_syncs_total", "Log syncs to disk.")?,
            events_published: counter(
                "belay_events_published_total",
                "Events published on the bus.",
            )?,
            event_lags: counter(
                "belay_event_lags_total",
                "Times a subscriber fell behind the replay buffer.",
            )?,
            event_subscribers: gauge("belay_event_subscribers", "Event stream subscribers.", &[])?,
            events_buffered: gauge("belay_events_buffered", "Events held for replay.", &[])?,
            cluster_members: gauge(
                "belay_cluster_members",
                "Cluster members by state, as this node sees them.",
                &["state"],
            )?,
            uptime: gauge(
                "belay_uptime_seconds",
                "Time since the engine started.",
                &[],
            )?,
            info: gauge(
                "belay_info",
                "Version and node id of this engine; always 1.",
                &["version", "node"],
            )?,
        })
    }

    fn all(&self) -> Vec<Desc> {
        vec![
            self.queue_jobs.clone(),
            self.queue_paused.clone(),
            self.oldest_ready.clone(),
            self.workers.clone(),
            self.lease_waiting.clone(),
            self.schedules.clone(),
            self.workflows_running.clone(),
            self.store_keys.clone(),
            self.store_live_bytes.clone(),
            self.store_wal_bytes.clone(),
            self.store_wal_segments.clone(),
            self.store_snapshot_bytes.clone(),
            self.store_last_seq.clone(),
            self.store_checkpoints.clone(),
            self.store_wal_syncs.clone(),
            self.events_published.clone(),
            self.event_lags.clone(),
            self.event_subscribers.clone(),
            self.events_buffered.clone(),
            self.cluster_members.clone(),
            self.uptime.clone(),
            self.info.clone(),
        ]
    }
}

/// Reads engine state at scrape time.
struct EngineCollector {
    shared: Weak<Shared>,
    descs: Descs,
}

impl EngineCollector {
    fn new(shared: Weak<Shared>) -> belay_metrics::Result<EngineCollector> {
        Ok(EngineCollector {
            shared,
            descs: Descs::new()?,
        })
    }
}

fn gauge(desc: &Desc, value: f64) -> FamilySnapshot {
    FamilySnapshot::new(desc).with_metric(&[], MetricValue::Gauge(value))
}

fn counter(desc: &Desc, value: u64) -> FamilySnapshot {
    FamilySnapshot::new(desc).with_metric(&[], MetricValue::Counter(value))
}

impl Collector for EngineCollector {
    fn describe(&self) -> Vec<Desc> {
        self.descs.all()
    }

    fn collect(&self) -> Vec<FamilySnapshot> {
        let Some(shared) = self.shared.upgrade() else {
            return Vec::new();
        };
        let d = &self.descs;
        let stats = shared.queue.stats();
        let mut jobs = FamilySnapshot::new(&d.queue_jobs);
        let mut paused = FamilySnapshot::new(&d.queue_paused);
        let mut oldest = FamilySnapshot::new(&d.oldest_ready);
        for q in &stats.queues {
            let name = q.name.as_str();
            for state in JobState::ALL {
                let count = q.counts.get(state) as f64;
                jobs = jobs.with_metric(
                    &[("queue", name), ("state", state.as_str())],
                    MetricValue::Gauge(count),
                );
            }
            paused = paused.with_metric(
                &[("queue", name)],
                MetricValue::Gauge(if q.paused { 1.0 } else { 0.0 }),
            );
            let age = q.oldest_ready_age.map_or(0.0, |a| a.as_secs_f64());
            oldest = oldest.with_metric(&[("queue", name)], MetricValue::Gauge(age));
        }

        let store = shared.kv.stats();
        let bus = shared.events.stats();
        let mut members = FamilySnapshot::new(&d.cluster_members);
        for (state, count) in shared.member_counts() {
            members = members.with_metric(&[("state", state)], MetricValue::Gauge(count as f64));
        }
        let info = FamilySnapshot::new(&d.info).with_metric(
            &[
                ("version", crate::VERSION),
                ("node", shared.node_id.as_str()),
            ],
            MetricValue::Gauge(1.0),
        );
        vec![
            jobs,
            paused,
            oldest,
            gauge(&d.workers, stats.workers as f64),
            gauge(&d.lease_waiting, stats.waiting_leases as f64),
            gauge(&d.schedules, shared.scheduler.len() as f64),
            gauge(
                &d.workflows_running,
                shared.workflows.engine().running() as f64,
            ),
            gauge(&d.store_keys, store.keys as f64),
            gauge(&d.store_live_bytes, store.live_bytes as f64),
            gauge(&d.store_wal_bytes, store.wal_bytes as f64),
            gauge(&d.store_wal_segments, store.wal_segments as f64),
            gauge(&d.store_snapshot_bytes, store.snapshot_bytes as f64),
            gauge(&d.store_last_seq, store.last_seq as f64),
            counter(&d.store_checkpoints, store.checkpoints),
            counter(&d.store_wal_syncs, store.wal_syncs),
            counter(&d.events_published, bus.published),
            counter(&d.event_lags, bus.lagged),
            gauge(&d.event_subscribers, bus.subscribers as f64),
            gauge(&d.events_buffered, bus.buffered as f64),
            members,
            gauge(&d.uptime, shared.uptime().as_secs_f64()),
            info,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Engine};

    fn engine(config: Config) -> Engine {
        Engine::builder()
            .config(config)
            .background(false)
            .start()
            .expect("engine starts")
    }

    #[test]
    fn every_listed_family_is_registered() {
        let engine = engine(Config::dev());
        let names: Vec<String> = engine
            .metrics()
            .snapshot()
            .into_iter()
            .map(|f| f.name)
            .collect();
        for family in FAMILIES {
            assert!(names.iter().any(|n| n == family), "{family} is missing");
        }
        assert!(names.iter().any(|n| n == "process_uptime_seconds"));
        let unlisted: Vec<&String> = names
            .iter()
            .filter(|n| !n.starts_with("process_") && !FAMILIES.contains(&n.as_str()))
            .collect();
        assert!(unlisted.is_empty(), "not in FAMILIES: {unlisted:?}");
    }

    #[test]
    fn disabled_metrics_leave_the_registry_empty() {
        let mut config = Config::dev();
        config.metrics.enabled = false;
        let engine = engine(config);
        assert!(engine.metrics().snapshot().is_empty());
        assert_eq!(engine.render_metrics(), "");
    }

    #[test]
    fn the_collector_outlives_its_engine_quietly() {
        let engine = engine(Config::dev());
        let registry = engine.metrics().clone();
        drop(engine);
        let text = registry.render_prometheus();
        assert!(!text.contains("belay_store_keys"), "{text}");
    }
}
