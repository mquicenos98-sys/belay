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

#[test]
fn a_failed_write_leaves_the_engine_unchanged() {
    let flaky = Arc::new(FlakyKv::new());
    let h = Harness::on(flaky.clone(), EngineOptions::new());
    let ready = h.enqueue(JobSpec::new("q", "ready"));
    let running_id = h.enqueue(JobSpec::new("q", "running").priority(1));
    let running = h.lease_one("q");
    assert_eq!(running.job.id, running_id);
    h.sink.take();
    let before = snapshot(&h);
    let counters = h.engine.stats().counters;

    flaky.fail_writes(true);
    let err = h.engine.enqueue(JobSpec::new("fresh", "k")).unwrap_err();
    assert!(matches!(err, Error::Store(_)));
    assert_eq!(err.code(), ErrorCode::Unavailable);
    assert!(h
        .engine
        .lease(&belay_proto::api::LeaseRequest::new(w("w2"), vec![q("q")]))
        .is_err());
    assert!(h
        .engine
        .complete(&running.job.id, &CompleteRequest::new(running.token))
        .is_err());
    assert!(h.engine.cancel(&ready).is_err());
    assert!(h.engine.pause(&q("q")).is_err());
    let later = h.secs(60);
    assert!(h.engine.tick(later).is_err(), "expiring the lease fails");

    // Compare at the time of the first snapshot: ages depend on the clock.
    h.clock.set(start());
    assert_eq!(snapshot(&h), before);
    h.clock.set(later);
    assert!(
        h.engine.queue_info(&q("fresh")).is_err(),
        "no queue half-created"
    );
    let after = h.engine.stats().counters;
    assert_eq!(
        (
            after.enqueued,
            after.leased,
            after.succeeded,
            after.cancelled
        ),
        (
            counters.enqueued,
            counters.leased,
            counters.succeeded,
            counters.cancelled
        )
    );
    assert!(
        h.events().iter().all(|e| *e == "worker.seen"),
        "no event for an unpersisted change"
    );
    h.assert_consistent();

    flaky.fail_writes(false);
    // The expired lease is reaped now; the ready job is still there.
    assert_eq!(h.tick().expired, 1);
    let l = h.lease_one("q");
    assert_eq!((l.job.id, l.job.attempt), (ready, 1));
    h.assert_consistent();
}

#[test]
fn ids_keep_increasing_across_a_restart_in_the_same_millisecond() {
    // Regression: a freshly seeded id generator could mint an id already
    // stored when restarted within the same millisecond, overwriting it.
    let mut h = Harness::new();
    let first: Vec<JobId> = (0..5).map(|_| h.enqueue(JobSpec::new("q", "k"))).collect();
    h.reopen();
    let second: Vec<JobId> = (0..5).map(|_| h.enqueue(JobSpec::new("q", "k"))).collect();
    let mut all = first.clone();
    all.extend(&second);
    let mut sorted = all.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted, all, "ids are distinct and in creation order");
    assert_eq!(h.all_jobs().len(), 10);
}

#[test]
fn ids_stay_ordered_when_the_clock_steps_back() {
    let h = Harness::new();
    let a = h.enqueue(JobSpec::new("q", "k"));
    h.clock.set(start() - Duration::from_secs(3600));
    let b = h.enqueue(JobSpec::new("q", "k"));
    assert!(b > a);
    assert_eq!(h.job(b).created_at, h.now(), "timestamps follow the clock");
}

#[test]
fn a_store_from_a_newer_version_is_refused() {
    let kv: Arc<dyn Kv> = Arc::new(MemKv::new());
    let schema = Namespace::new("qmeta").key_buf().str("schema").into_vec();
    kv.put(&schema, b"2").expect("put");
    let clock = belay_common::ManualClock::new(start());
    let err = QueueEngine::open(
        kv,
        clock.shared(),
        belay_proto::source::NoopSink::shared(),
        EngineOptions::new(),
    )
    .unwrap_err();
    assert!(matches!(err, Error::FailedPrecondition(_)), "{err}");
    assert!(err.to_string().contains("format 2"), "{err}");
}

#[test]
fn a_corrupt_job_document_stops_the_open_with_its_key() {
    let mut h = Harness::new();
    let id = h.enqueue(JobSpec::new("q", "k"));
    let key = Namespace::new("job").key_buf().ulid(id.ulid()).into_vec();
    h.kv.put(&key, b"{\"id\": 1").expect("put");
    let err = QueueEngine::open(
        Arc::clone(&h.kv),
        h.clock.shared(),
        belay_proto::source::NoopSink::shared(),
        EngineOptions::new(),
    )
    .unwrap_err();
    assert!(matches!(err, Error::Corrupt { .. }), "{err}");
    // Put it back and all is well.
    h.kv.delete(&key).expect("delete");
    h.reopen();
    assert!(h.all_jobs().is_empty());
}

#[test]
fn stale_claims_and_missing_queue_records_are_repaired_on_open() {
    let mut h = Harness::new();
    let id = h.enqueue(JobSpec::new("q", "k").unique("kept", UniqueScope::UntilFinished));
    let ghost = JobId::generate();
    let claim = |key: &str| {
        Namespace::new("uniq")
            .key_buf()
            .str("q")
            .str(key)
            .into_vec()
    };
    h.kv.put(
        &claim("stale"),
        format!("{{\"job\":\"{ghost}\"}}").as_bytes(),
    )
    .expect("put");
    let queue_key = Namespace::new("queue").key_buf().str("q").into_vec();
    h.kv.delete(&queue_key).expect("delete");
    h.reopen();
    assert!(
        h.kv.get(&claim("stale")).expect("get").is_none(),
        "stale claim deleted"
    );
    assert!(h.kv.get(&claim("kept")).expect("get").is_some());
    assert!(
        h.kv.get(&queue_key).expect("get").is_some(),
        "queue record restored"
    );
    let dup = h
        .engine
        .enqueue(JobSpec::new("q", "k").unique("kept", UniqueScope::UntilFinished))
        .expect("enqueue");
    assert_eq!(dup.id, id);
    h.assert_consistent();
}

#[test]
fn verify_reports_a_store_changed_behind_the_engines_back() {
    let h = Harness::new();
    let id = h.enqueue(JobSpec::new("q", "k"));
    h.assert_consistent();
    let key = Namespace::new("job").key_buf().ulid(id.ulid()).into_vec();
    h.kv.delete(&key).expect("delete");
    let report = h.engine.verify().expect("verify");
    assert!(!report.is_consistent());
    assert!(
        report
            .problems
            .iter()
            .any(|p| p.contains("indexed but not stored")),
        "{:?}",
        report.problems
    );
    // Reads report the damage instead of panicking.
    assert!(h.engine.cancel(&id).is_err());
}

#[test]
fn engines_in_separate_namespaces_share_a_store_without_seeing_each_other() {
    let kv: Arc<dyn Kv> = Arc::new(MemKv::new());
    let a = Harness::on(
        Arc::clone(&kv),
        EngineOptions::new().namespace(Namespace::new("a")),
    );
    let b = Harness::on(
        Arc::clone(&kv),
        EngineOptions::new().namespace(Namespace::new("b")),
    );
    a.enqueue(JobSpec::new("q", "from-a"));
    assert!(b.all_jobs().is_empty());
    assert!(b.lease("q", 1).is_empty());
    b.enqueue(JobSpec::new("q", "from-b"));
    assert_eq!(a.lease_one("q").job.spec.kind, "from-a");
    a.assert_consistent();
    b.assert_consistent();
}
