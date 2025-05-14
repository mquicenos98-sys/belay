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
