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

#[test]
fn weighted_tenants_get_proportional_turns() {
    let h = Harness::with_options(EngineOptions::new().tenant_weight(tenant("gold"), 3));
    for i in 0..6 {
        h.enqueue(JobSpec::new("q", format!("gold{i}")).tenant(tenant("gold")));
        h.enqueue(JobSpec::new("q", format!("free{i}")).tenant(tenant("free")));
    }
    assert_eq!(
        kinds(&h, &["q"], 8),
        ["gold0", "gold1", "gold2", "free0", "gold3", "gold4", "gold5", "free1"]
    );
}

#[test]
fn priority_orders_jobs_within_a_tenant_only() {
    let h = Harness::new();
    h.enqueue(JobSpec::new("q", "a-low").tenant(tenant("a")));
    h.enqueue(JobSpec::new("q", "a-high").tenant(tenant("a")).priority(50));
    h.enqueue(
        JobSpec::new("q", "b-urgent")
            .tenant(tenant("b"))
            .priority(1000),
    );
    assert_eq!(kinds(&h, &["q"], 3), ["a-high", "b-urgent", "a-low"]);
}

#[test]
fn jobs_without_a_tenant_share_one_turn() {
    let h = Harness::new();
    h.enqueue(JobSpec::new("q", "anon0"));
    h.enqueue(JobSpec::new("q", "anon1"));
    h.enqueue(JobSpec::new("q", "t0").tenant(tenant("t")));
    assert_eq!(kinds(&h, &["q"], 3), ["anon0", "t0", "anon1"]);
    let stats = h.engine.stats();
    assert_eq!(stats.queue("q").map(|s| s.tenants), Some(0));
}

#[test]
fn max_concurrency_caps_running_jobs() {
    let h = Harness::new();
    configure(&h, "q", |c| c.max_concurrency = Some(2));
    fill(&h, "q", 5);
    let first = h.lease("q", 5);
    assert_eq!(first.len(), 2);
    assert!(h.lease("q", 1).is_empty());
    h.complete(&first[0]);
    assert_eq!(h.lease("q", 5).len(), 1);
    h.fail(&first[1], true);
    assert_eq!(h.lease("q", 5).len(), 1, "a failed attempt frees its slot");
    // An expired lease frees a slot too.
    h.secs(30);
    assert_eq!(h.tick().expired, 2);
    assert_eq!(h.lease("q", 5).len(), 2);
    h.assert_consistent();
}

#[test]
fn raising_the_concurrency_limit_applies_at_once() {
    let h = Harness::new();
    configure(&h, "q", |c| c.max_concurrency = Some(1));
    fill(&h, "q", 3);
    assert_eq!(h.lease("q", 3).len(), 1);
    configure(&h, "q", |c| c.max_concurrency = Some(3));
    assert_eq!(h.lease("q", 3).len(), 2);
}

#[test]
fn rate_limit_allows_a_burst_then_refills_with_the_clock() {
    let h = Harness::new();
    configure(&h, "q", |c| c.rate_limit = Some(RateLimit::new(2.0, 2)));
    fill(&h, "q", 10);
    assert_eq!(h.lease("q", 10).len(), 2);
    assert!(h.lease("q", 10).is_empty());
    h.advance(Duration::from_millis(499));
    assert!(h.lease("q", 10).is_empty());
    h.advance(Duration::from_millis(1));
    assert_eq!(h.lease("q", 10).len(), 1);
    // A long idle period refills only up to the burst.
    h.secs(60);
    assert_eq!(h.lease("q", 10).len(), 2);
    let tokens = h.engine.stats().queue("q").and_then(|s| s.rate_tokens);
    assert!(tokens.is_some_and(|t| t < 1.0), "{tokens:?}");
}

#[test]
fn fractional_rates_space_grants_out() {
    let h = Harness::new();
    configure(&h, "q", |c| c.rate_limit = Some(RateLimit::new(0.5, 1)));
    fill(&h, "q", 3);
    let mut granted_at = Vec::new();
    for second in 0..6 {
        if !h.lease("q", 1).is_empty() {
            granted_at.push(second);
        }
        h.secs(1);
    }
    assert_eq!(granted_at, [0, 2, 4]);
}

#[test]
fn an_idle_rate_limited_queue_keeps_its_tokens() {
    let h = Harness::new();
    configure(&h, "q", |c| c.rate_limit = Some(RateLimit::new(1.0, 3)));
    for _ in 0..5 {
        assert!(h.lease("q", 3).is_empty());
    }
    fill(&h, "q", 3);
    assert_eq!(h.lease("q", 3).len(), 3);
}

#[test]
fn rate_and_concurrency_limits_combine() {
    let h = Harness::new();
    configure(&h, "q", |c| {
        c.rate_limit = Some(RateLimit::new(100.0, 100));
        c.max_concurrency = Some(1);
    });
    fill(&h, "q", 3);
    let l = h.lease("q", 3);
    assert_eq!(l.len(), 1);
    // The refused picks did not spend tokens.
    let tokens = h.engine.stats().queue("q").and_then(|s| s.rate_tokens);
    assert!(
        tokens.is_some_and(|t| (t - 99.0).abs() < 1e-6),
        "{tokens:?}"
    );
    h.complete(&l[0]);
    assert_eq!(h.lease("q", 3).len(), 1);
}

#[test]
fn a_paused_queue_accepts_jobs_but_hands_none_out() {
    let h = Harness::new();
    h.engine.pause(&q("q")).expect("pause");
    let id = h.enqueue(JobSpec::new("q", "k"));
    fill(&h, "other", 1);
    assert_eq!(kinds(&h, &["q", "other"], 5), ["other0"]);
    let info = h.engine.queue_info(&q("q")).expect("info");
    assert!(info.paused);
    assert_eq!(info.counts.ready, 1);
    h.engine.resume(&q("q")).expect("resume");
    assert_eq!(h.lease_one("q").job.id, id);
}

#[test]
fn pausing_before_the_first_job_creates_the_queue() {
    let mut h = Harness::new();
    h.engine.pause(&q("later")).expect("pause");
    h.reopen();
    let info = h.engine.queue_info(&q("later")).expect("queue exists");
    assert!(info.paused, "the pause is persisted");
}

#[test]
fn max_jobs_is_honoured_across_queues() {
    let h = Harness::new();
    fill(&h, "a", 5);
    fill(&h, "b", 5);
    fill(&h, "c", 5);
    assert_eq!(h.lease_from("w1", &["a", "b", "c"], 7).len(), 7);
    assert_eq!(h.lease_from("w1", &["a", "b", "c"], 100).len(), 8);
}

#[test]
fn leases_by_different_workers_never_share_a_job() {
    let h = Harness::new();
    fill(&h, "q", 50);
    let mut seen = std::collections::HashSet::new();
    for worker in ["w1", "w2", "w3", "w4", "w5"].iter().cycle().take(20) {
        for l in h.lease_from(worker, &["q"], 3) {
            assert!(seen.insert(l.job.id), "job {} leased twice", l.job.id);
        }
    }
    assert_eq!(seen.len(), 50);
    let workers = h.engine.workers();
    assert_eq!(workers.len(), 5);
    assert_eq!(workers.iter().map(|w| w.active_jobs).sum::<u32>(), 50);
}
