//! Who gets which job: order across queues, fair share between tenants,
//! priorities, and the limits that hold jobs back (pause, concurrency,
//! rate).

mod support;

use std::time::Duration;

use belay_proto::{JobId, JobSpec, QueueConfig, RateLimit, TenantId};
use belay_queue::{EngineOptions, QueueOrder};
use support::*;

fn tenant(name: &str) -> TenantId {
    name.parse().expect("valid tenant")
}

fn kinds(h: &Harness, queues: &[&'static str], n: u32) -> Vec<String> {
    h.lease_from("w1", queues, n)
        .into_iter()
        .map(|l| l.job.spec.kind)
        .collect()
}

fn fill(h: &Harness, queue: &'static str, n: usize) -> Vec<JobId> {
    (0..n)
        .map(|i| h.enqueue(JobSpec::new(queue, format!("{queue}{i}"))))
        .collect()
}

fn configure(h: &Harness, queue: &'static str, edit: impl FnOnce(&mut QueueConfig)) {
    let mut config = QueueConfig::default();
    edit(&mut config);
    h.engine.set_config(&q(queue), config).expect("set config");
}

#[test]
fn a_flooded_queue_does_not_starve_the_others() {
    let h = Harness::new();
    fill(&h, "bulk", 10);
    fill(&h, "urgent", 2);
    assert_eq!(
        kinds(&h, &["bulk", "urgent"], 4),
        ["bulk0", "urgent0", "bulk1", "urgent1"]
    );
    // With one queue exhausted the rest of the batch comes from the other.
    assert_eq!(
        kinds(&h, &["bulk", "urgent"], 3),
        ["bulk2", "bulk3", "bulk4"]
    );
}

#[test]
fn single_job_requests_rotate_their_starting_queue() {
    let h = Harness::new();
    fill(&h, "a", 3);
    fill(&h, "b", 3);
    let order: Vec<String> = (0..4).flat_map(|_| kinds(&h, &["a", "b"], 1)).collect();
    assert_eq!(order, ["a0", "b0", "a1", "b1"]);
}

#[test]
fn in_order_drains_queues_by_preference() {
    let h = Harness::with_options(EngineOptions::new().queue_order(QueueOrder::InOrder));
    fill(&h, "first", 2);
    fill(&h, "second", 2);
    assert_eq!(
        kinds(&h, &["first", "second"], 3),
        ["first0", "first1", "second0"]
    );
    assert_eq!(kinds(&h, &["second", "first"], 1), ["second1"]);
}

#[test]
fn unknown_queues_are_skipped_and_not_created() {
    let h = Harness::new();
    fill(&h, "real", 1);
    assert_eq!(kinds(&h, &["ghost", "real"], 5), ["real0"]);
    assert!(h.lease("ghost", 1).is_empty());
    let names: Vec<String> = h
        .engine
        .list_queues()
        .into_iter()
        .map(|info| info.name.to_string())
        .collect();
    assert_eq!(names, ["real"]);
}

#[test]
fn higher_priority_runs_first_then_the_longest_waiting() {
    let h = Harness::new();
    let low = h.enqueue(JobSpec::new("q", "low").priority(-5));
    h.secs(1);
    let normal_late = h.enqueue(JobSpec::new("q", "normal-late"));
    let high = h.enqueue(JobSpec::new("q", "high").priority(100));
    // Enqueued later but eligible earlier than normal-late.
    let normal_early = h.enqueue(JobSpec::new("q", "normal-early").run_at(start()));
    let ids: Vec<JobId> = h.lease("q", 10).into_iter().map(|l| l.job.id).collect();
    assert_eq!(ids, [high, normal_early, normal_late, low]);
}

#[test]
fn equal_jobs_go_in_id_order() {
    let h = Harness::new();
    let ids = fill(&h, "q", 20);
    let leased: Vec<JobId> = h.lease("q", 20).into_iter().map(|l| l.job.id).collect();
    assert_eq!(leased, ids);
}

#[test]
fn a_due_timer_competes_on_priority_with_ready_jobs() {
    let h = Harness::new();
    fill(&h, "q", 2);
    let urgent = h.enqueue(
        JobSpec::new("q", "urgent")
            .priority(10)
            .delay(Duration::from_secs(5)),
    );
    h.secs(5);
    // No tick: the lease promotes the due job and ranks it by priority.
    assert_eq!(h.lease_one("q").job.id, urgent);
}

#[test]
fn tenants_take_turns_within_a_queue() {
    let h = Harness::new();
    for i in 0..6 {
        h.enqueue(JobSpec::new("q", format!("big{i}")).tenant(tenant("big")));
    }
    for i in 0..2 {
        h.enqueue(JobSpec::new("q", format!("small{i}")).tenant(tenant("small")));
    }
    assert_eq!(
        kinds(&h, &["q"], 6),
        ["big0", "small0", "big1", "small1", "big2", "big3"]
    );
}

#[test]
fn tenant_turns_carry_over_between_requests() {
    let h = Harness::new();
    for i in 0..3 {
        h.enqueue(JobSpec::new("q", format!("a{i}")).tenant(tenant("a")));
        h.enqueue(JobSpec::new("q", format!("b{i}")).tenant(tenant("b")));
    }
    let order: Vec<String> = (0..6).flat_map(|_| kinds(&h, &["q"], 1)).collect();
    assert_eq!(order, ["a0", "b0", "a1", "b1", "a2", "b2"]);
}
