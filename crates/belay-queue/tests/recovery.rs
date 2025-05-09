//! Durability: what survives a restart or a crash, how the indexes are
//! rebuilt, and what a failed store write leaves behind (nothing).

mod support;

use std::sync::Arc;
use std::time::Duration;

use belay_common::{BackoffPolicy, Rng};
use belay_json::json;
use belay_proto::api::{CompleteRequest, ErrorCode, HeartbeatRequest, LeasedJob};
use belay_proto::{Job, JobId, JobSpec, JobState, QueueConfig, StateCounts, TenantId, UniqueScope};
use belay_queue::{EngineOptions, Error, QueueEngine};
use belay_store::vfs::{Crash, MemFs};
use belay_store::{Kv, LogKv, MemKv, Namespace, Options, SyncPolicy};
use support::*;

/// What must survive: every job as stored, and each queue's persisted
/// settings and counts. Throughput windows and the worker registry are
/// memory-only by design and left out.
#[derive(Debug, PartialEq)]
struct Snapshot {
    jobs: Vec<Job>,
    queues: Vec<(String, bool, QueueConfig, StateCounts, Option<Duration>)>,
    claims: Vec<(String, usize)>,
}

fn snapshot(h: &Harness) -> Snapshot {
    Snapshot {
        jobs: h.all_jobs(),
        queues: h
            .engine
            .list_queues()
            .into_iter()
            .map(|i| {
                (
                    i.name.to_string(),
                    i.paused,
                    i.config,
                    i.counts,
                    i.oldest_ready_age,
                )
            })
            .collect(),
        claims: h
            .engine
            .stats()
            .queues
            .into_iter()
            .map(|s| (s.name.to_string(), s.unique_keys))
            .collect(),
    }
}

/// A deterministic workload: each step is one acknowledged operation,
/// chosen by a seeded generator from what the engine currently holds.
struct Workload {
    rng: Rng,
    leased: Vec<LeasedJob>,
    ids: Vec<JobId>,
}

impl Workload {
    fn new() -> Workload {
        Workload {
            rng: Rng::seed_from_u64(42),
            leased: Vec::new(),
            ids: Vec::new(),
        }
    }

    fn step(&mut self, h: &Harness) {
        let queues = ["mail", "sms"];
        let queue = queues[self.rng.gen_range(0..2u64) as usize];
        match self.rng.gen_range(0..12u64) {
            0..=2 => {
                let mut spec = JobSpec::new(queue, "send")
                    .payload(json!({"n": self.ids.len()}))
                    .priority(self.rng.gen_range(0..3u64) as i16)
                    .max_attempts(2)
                    .retry(BackoffPolicy::constant(Duration::from_secs(5)));
                if self.rng.gen_bool(0.3) {
                    spec = spec.delay(Duration::from_secs(self.rng.gen_range(1..20u64)));
                }
                if self.rng.gen_bool(0.3) {
                    let key = format!("u{}", self.rng.gen_range(0..4u64));
                    spec = spec.unique(key, UniqueScope::UntilFinished);
                }
                if self.rng.gen_bool(0.3) {
                    spec = spec.tenant("acme".parse().expect("tenant"));
                }
                let result = h.engine.enqueue(spec).expect("enqueue");
                self.ids.push(result.id);
            }
            3 | 4 => {
                let n = self.rng.gen_range(1..3u64) as u32;
                self.leased.extend(h.lease_from("w1", &[queue], n));
            }
            5 if !self.leased.is_empty() => {
                let l = self.take();
                let _ = h.engine.complete(&l.job.id, &CompleteRequest::new(l.token));
            }
            6 if !self.leased.is_empty() => {
                let l = self.take();
                let retryable = self.rng.gen_bool(0.7);
                let mut req = belay_proto::api::FailRequest::new(l.token, "boom");
                req.retryable = retryable;
                let _ = h.engine.fail(&l.job.id, &req);
            }
            7 if !self.leased.is_empty() => {
                let i = self.rng.gen_range(0..self.leased.len() as u64) as usize;
                let l = &self.leased[i];
                let _ = h
                    .engine
                    .heartbeat(&l.job.id, &HeartbeatRequest::new(l.token));
            }
            8 if !self.ids.is_empty() => {
                let id = self.ids[self.rng.gen_range(0..self.ids.len() as u64) as usize];
                let _ = match self.rng.gen_range(0..4u64) {
                    0 => h.engine.cancel(&id).map(drop),
                    1 => h.engine.retry(&id).map(drop),
                    2 => h.engine.promote(&id).map(drop),
                    _ => h.engine.delete(&id),
                };
            }
            9 => {
                if self.rng.gen_bool(0.5) {
                    h.engine.pause(&q(queue)).expect("pause");
                } else {
                    h.engine.resume(&q(queue)).expect("resume");
                }
            }
            10 => {
                let mut config = QueueConfig::default();
                config.max_concurrency = Some(self.rng.gen_range(1..5u64) as u32);
                config.retention.succeeded = Duration::from_secs(30);
                h.engine.set_config(&q(queue), config).expect("config");
            }
            _ => {
                h.secs(self.rng.gen_range(1..15u64));
                h.tick();
            }
        }
    }

    fn take(&mut self) -> LeasedJob {
        let i = self.rng.gen_range(0..self.leased.len() as u64) as usize;
        self.leased.swap_remove(i)
    }
}

fn durable_harness(fs: &MemFs) -> Harness {
    Harness::on(durable(fs), EngineOptions::new())
}

#[test]
fn everything_acknowledged_survives_a_clean_restart() {
    let fs = MemFs::new();
    let mut h = durable_harness(&fs);
    let mut work = Workload::new();
    for _ in 0..150 {
        work.step(&h);
    }
    let before = snapshot(&h);
    assert!(before.jobs.len() > 20, "the workload should build up jobs");
    h.restart(|| durable(&fs));
    assert_eq!(snapshot(&h), before);
    h.assert_consistent();
}

#[test]
fn a_power_loss_keeps_every_acknowledged_operation() {
    // Reference run: the state after each step.
    let reference_fs = MemFs::new();
    let h = durable_harness(&reference_fs);
    let mut work = Workload::new();
    let mut expected = vec![snapshot(&h)];
    for _ in 0..120 {
        work.step(&h);
        expected.push(snapshot(&h));
    }
    drop(h);

    for crash_after in [0usize, 1, 7, 23, 58, 91, 120] {
        let fs = MemFs::new();
        let mut h = durable_harness(&fs);
        let mut work = Workload::new();
        for _ in 0..crash_after {
            work.step(&h);
        }
        assert_eq!(snapshot(&h), expected[crash_after], "replay diverged");
        fs.crash(&Crash::PowerLoss);
        h.restart(|| durable(&fs));
        assert_eq!(
            snapshot(&h),
            expected[crash_after],
            "state after a crash at step {crash_after}"
        );
        h.assert_consistent();
        // The recovered engine keeps working, tokens included.
        for _ in 0..10 {
            work.step(&h);
        }
        h.assert_consistent();
    }
}

#[test]
fn a_process_crash_loses_nothing_even_without_syncing() {
    // Handles from before a crash stop working, so each open takes a
    // fresh clone of the file system.
    let unsynced = |fs: &MemFs| -> Arc<dyn Kv> {
        let opts = Options::new()
            .fs(fs.clone())
            .sync(SyncPolicy::Never)
            .manual_checkpoints();
        Arc::new(LogKv::open("store", opts).expect("open"))
    };
    quiet_logs();
    let fs = MemFs::new();
    let mut h = Harness::on(unsynced(&fs), EngineOptions::new());
    let mut work = Workload::new();
    for _ in 0..80 {
        work.step(&h);
    }
    let before = snapshot(&h);
    fs.crash(&Crash::ProcessExit);
    h.restart(|| unsynced(&fs));
    assert_eq!(snapshot(&h), before);
}

#[test]
fn running_jobs_keep_their_lease_tokens_across_a_restart() {
    let fs = MemFs::new();
    let mut h = durable_harness(&fs);
    let id = h.enqueue(JobSpec::new("q", "k"));
    let l = h.lease_one("q");
    h.restart(|| durable(&fs));
    let workers = h.engine.workers();
    assert_eq!(workers.len(), 1, "seeded from running leases");
    assert_eq!(workers[0].active_jobs, 1);
    h.heartbeat(&l);
    h.complete(&l);
    assert_eq!(h.state(id), JobState::Succeeded);
}

#[test]
fn indexes_are_rebuilt_from_thousands_of_jobs() {
    let fs = MemFs::new();
    let mut h = durable_harness(&fs);
    let tenants: Vec<TenantId> = ["a", "b", "c", "d"]
        .iter()
        .map(|t| t.parse().expect("tenant"))
        .collect();
    let queues = ["alpha", "beta", "gamma"];
    let mut rng = Rng::seed_from_u64(7);
    let mut specs = Vec::new();
    for i in 0..3000u64 {
        let queue = queues[(i % 3) as usize];
        let mut spec = JobSpec::new(queue, "k")
            .priority(rng.gen_range(0..5u64) as i16)
            .tenant(tenants[rng.gen_range(0..4u64) as usize].clone());
        if i % 6 == 0 {
            spec = spec.delay(Duration::from_secs(rng.gen_range(10..1000u64)));
        }
        if i % 10 == 0 {
            spec = spec.unique(format!("key{i}"), UniqueScope::UntilFinished);
        }
        specs.push(spec);
        if specs.len() == 500 {
            h.engine
                .enqueue_batch(std::mem::take(&mut specs))
                .expect("batch");
        }
    }
    let leased = h.lease_from("w1", &queues, 100);
    for (i, l) in leased.iter().enumerate() {
        match i % 4 {
            0 => h.complete(l),
            1 => {
                h.fail(l, true);
            }
            2 => {
                h.fail(l, false);
            }
            _ => {}
        }
    }
    for id in h.all_jobs().iter().step_by(37).map(|j| j.id) {
        let _ = h.engine.cancel(&id);
    }
    h.secs(20);
    h.tick();
    let before = snapshot(&h);
    let stats_before = h.engine.stats();
    assert_eq!(stats_before.totals.total(), 3000);

    h.restart(|| durable(&fs));
    assert_eq!(snapshot(&h), before);
    let stats = h.engine.stats();
    assert_eq!(stats.totals, stats_before.totals);
    h.assert_consistent();

    // The rebuilt ready sets hand jobs out by priority within each tenant.
    let mut last_priority: std::collections::HashMap<(String, Option<TenantId>), i16> =
        std::collections::HashMap::new();
    let ready = stats.totals.ready as usize;
    let mut leased = 0;
    loop {
        let batch = h.lease_from("w2", &queues, 100);
        if batch.is_empty() {
            break;
        }
        for l in batch {
            leased += 1;
            let key = (l.job.spec.queue.to_string(), l.job.spec.tenant.clone());
            let p = l.job.spec.priority;
            if let Some(prev) = last_priority.insert(key, p) {
                assert!(p <= prev, "priority went up within a tenant flow");
            }
        }
    }
    assert!(
        leased >= ready,
        "every ready job is leasable after the rebuild"
    );
}
