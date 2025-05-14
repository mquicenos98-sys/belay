//! Time-driven changes made by `tick`: promotion, lease expiry,
//! retention, timed claims and worker liveness.

mod support;

use std::time::Duration;

use belay_common::BackoffPolicy;
use belay_proto::event::EventKind;
use belay_proto::{JobSpec, JobState, QueueConfig, Retention};
use belay_queue::{EngineOptions, TickReport};
use support::*;

fn retention(h: &Harness, queue: &'static str, keep: Retention) {
    let mut config = QueueConfig::default();
    config.retention = keep;
    h.engine.set_config(&q(queue), config).expect("config");
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

#[test]
fn a_tick_with_nothing_due_does_nothing() {
    let h = Harness::new();
    h.enqueue(JobSpec::new("q", "later").delay(secs(60)));
    h.lease_from("w1", &["other"], 1);
    h.sink.take();
    assert_eq!(h.tick(), TickReport::default());
    assert!(h.tick().is_idle());
    assert!(h.events().is_empty());
}

#[test]
fn scheduled_jobs_become_ready_exactly_at_run_at() {
    let h = Harness::new();
    let id = h.enqueue(JobSpec::new("q", "k").delay(secs(10)));
    h.advance(Duration::from_millis(9_999));
    assert_eq!(h.tick().promoted, 0);
    assert_eq!(h.state(id), JobState::Scheduled);
    h.advance(Duration::from_millis(1));
    assert_eq!(h.tick().promoted, 1);
    let job = h.job(id);
    assert_eq!((job.state, job.run_at), (JobState::Ready, h.now()));
    let info = h.engine.queue_info(&q("q")).expect("info");
    assert_eq!(info.oldest_ready_age, Some(Duration::ZERO));
}

#[test]
fn expired_leases_retry_after_backoff_and_die_when_exhausted() {
    let h = Harness::new();
    let id = h.enqueue(
        JobSpec::new("q", "k")
            .max_attempts(2)
            .retry(BackoffPolicy::constant(secs(5))),
    );
    h.lease_one("q");
    h.secs(30);
    let report = h.tick();
    assert_eq!((report.expired, report.promoted), (1, 0));
    assert_eq!(h.job(id).run_at, h.now() + secs(5));
    h.secs(5);
    assert_eq!(h.tick().promoted, 1);
    h.lease_one("q");
    h.secs(30);
    assert_eq!(h.tick().expired, 1);
    let job = h.job(id);
    assert_eq!(job.state, JobState::Dead);
    assert_eq!(job.errors.len(), 2);
    assert_eq!(h.engine.stats().counters.expired, 2);
}

#[test]
fn a_heartbeating_worker_keeps_its_lease() {
    let h = Harness::new();
    let id = h.enqueue(JobSpec::new("q", "k"));
    let l = h.lease_one("q");
    for _ in 0..10 {
        h.secs(20);
        h.heartbeat(&l);
        assert_eq!(h.tick().expired, 0);
    }
    assert_eq!(h.state(id), JobState::Running);
    assert_eq!(h.engine.workers()[0].active_jobs, 1);
}

#[test]
fn many_expired_leases_are_reaped_in_one_tick() {
    let h = Harness::with_options(EngineOptions::new().batch_size(4));
    for i in 0..10 {
        h.enqueue(JobSpec::new("q", format!("k{i}")));
    }
    assert_eq!(h.lease("q", 10).len(), 10);
    h.secs(31);
    assert_eq!(h.tick().expired, 10);
    let info = h.engine.queue_info(&q("q")).expect("info");
    assert_eq!((info.counts.running, info.counts.retrying), (0, 10));
    h.assert_consistent();
}

#[test]
fn finished_jobs_are_deleted_after_their_retention() {
    let h = Harness::new();
    retention(
        &h,
        "q",
        Retention {
            succeeded: secs(60),
            dead: secs(600),
            cancelled: Duration::ZERO,
        },
    );
    let ok = h.enqueue(JobSpec::new("q", "ok"));
    let l = h.lease_one("q");
    h.complete(&l);
    let dead = h.enqueue(JobSpec::new("q", "dead"));
    let l = h.lease_one("q");
    h.fail(&l, false);
    let cancelled = h.enqueue(JobSpec::new("q", "cancelled"));
    h.engine.cancel(&cancelled).expect("cancel");
    h.sink.take();

    assert_eq!(h.tick().deleted, 1, "zero retention deletes at once");
    assert!(h.engine.get(&cancelled).is_err());
    h.secs(59);
    assert_eq!(h.tick().deleted, 0);
    h.secs(1);
    assert_eq!(h.tick().deleted, 1);
    assert!(h.engine.get(&ok).is_err());
    assert_eq!(h.state(dead), JobState::Dead);
    h.secs(540);
    assert_eq!(h.tick().deleted, 1);
    assert!(h.all_jobs().is_empty());
    let deleted: Vec<_> = h
        .sink
        .take()
        .into_iter()
        .filter(|e| matches!(e, EventKind::JobDeleted { .. }))
        .collect();
    assert_eq!(deleted.len(), 3);
    h.assert_consistent();
}

#[test]
fn shortening_retention_applies_to_jobs_already_finished() {
    let h = Harness::new();
    let id = h.enqueue(JobSpec::new("q", "k"));
    let l = h.lease_one("q");
    h.complete(&l);
    h.secs(120);
    assert_eq!(h.tick().deleted, 0, "default keeps succeeded jobs a day");
    retention(
        &h,
        "q",
        Retention {
            succeeded: secs(60),
            ..Retention::default()
        },
    );
    assert_eq!(h.tick().deleted, 1);
    assert!(h.engine.get(&id).is_err());
}

#[test]
fn retention_works_off_a_backlog_over_several_ticks() {
    let h = Harness::with_options(EngineOptions::new().batch_size(3).retention_per_tick(10));
    retention(
        &h,
        "q",
        Retention {
            cancelled: secs(1),
            ..Retention::default()
        },
    );
    for i in 0..25 {
        let id = h.enqueue(JobSpec::new("q", format!("k{i}")));
        h.engine.cancel(&id).expect("cancel");
    }
    h.secs(1);
    let first = h.tick();
    assert_eq!((first.deleted, first.backlog), (10, true));
    let second = h.tick();
    assert_eq!((second.deleted, second.backlog), (10, true));
    let third = h.tick();
    assert_eq!((third.deleted, third.backlog), (5, false));
    assert!(h.all_jobs().is_empty());
}

#[test]
fn retention_never_touches_pending_jobs() {
    let h = Harness::new();
    retention(
        &h,
        "q",
        Retention {
            succeeded: Duration::ZERO,
            dead: Duration::ZERO,
            cancelled: Duration::ZERO,
        },
    );
    let ready = h.enqueue(JobSpec::new("q", "ready"));
    let scheduled = h.enqueue(JobSpec::new("q", "later").delay(secs(3600)));
    h.secs(100);
    assert_eq!(h.tick().deleted, 0);
    assert_eq!(h.state(scheduled), JobState::Scheduled);
    assert_eq!(h.state(ready), JobState::Ready);
}

#[test]
fn silent_workers_are_reported_lost_and_announced_on_return() {
    let h = Harness::with_options(
        EngineOptions::new()
            .worker_timeout(secs(60))
            .worker_retention(secs(600)),
    );
    h.lease_from("w1", &["q"], 1);
    h.lease_from("w2", &["q"], 1);
    h.sink.take();
    h.secs(30);
    h.lease_from("w2", &["q"], 1);
    h.secs(30);
    let report = h.tick();
    assert_eq!(report.workers_lost, 1);
    match h.sink.take().as_slice() {
        [EventKind::WorkerLost { worker, last_seen }] => {
            assert_eq!((worker.as_str(), *last_seen), ("w1", start()));
        }
        other => panic!("expected one worker.lost, got {other:?}"),
    }
    assert_eq!(h.tick().workers_lost, 0, "reported once");
    h.lease_from("w1", &["q"], 1);
    assert_eq!(h.events(), ["worker.seen"]);
    // Silent past the retention period with nothing held: forgotten.
    h.secs(3600);
    h.tick();
    assert!(h.engine.workers().is_empty());
}

#[test]
fn a_worker_holding_jobs_is_not_forgotten() {
    let h = Harness::with_options(
        EngineOptions::new()
            .worker_timeout(secs(60))
            .worker_retention(secs(60)),
    );
    h.enqueue(JobSpec::new("q", "k").timeout(Duration::from_secs(7 * 86_400)));
    let req = belay_proto::api::LeaseRequest::new(w("slow"), vec![q("q")])
        .lease_for(Duration::from_secs(3600));
    h.engine.lease(&req).expect("lease");
    h.secs(600);
    h.tick();
    let workers = h.engine.workers();
    assert_eq!(workers.len(), 1);
    assert_eq!(workers[0].active_jobs, 1);
}

#[test]
fn tick_uses_the_time_it_is_given() {
    let h = Harness::new();
    let id = h.enqueue(JobSpec::new("q", "k").delay(secs(60)));
    // The engine clock has not moved, but the caller says it is later.
    let report = h.engine.tick(h.now() + secs(60)).expect("tick");
    assert_eq!(report.promoted, 1);
    assert_eq!(h.state(id), JobState::Ready);
}
