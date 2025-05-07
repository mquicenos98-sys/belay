//! Reads and administration: filtered listings with stable pagination,
//! queue and worker reports, metrics, validation, purge and queue
//! deletion.

mod support;

use std::time::Duration;

use belay_proto::api::{ApiError, CompleteRequest, Cursor, ErrorCode, JobFilter, SortOrder};
use belay_proto::{JobId, JobSpec, JobState, QueueConfig, RateLimit, TenantId};
use belay_queue::{EngineOptions, Error};
use support::*;

fn ids(page: &belay_proto::api::Page<belay_proto::Job>) -> Vec<JobId> {
    page.items.iter().map(|j| j.id).collect()
}

fn tenant(name: &str) -> TenantId {
    name.parse().expect("tenant")
}

/// Pages through `filter` with `limit`, returning every id in order.
fn page_through(h: &Harness, filter: JobFilter, limit: u32) -> Vec<JobId> {
    let mut filter = filter.limit(limit);
    let mut out = Vec::new();
    loop {
        let page = h.engine.list(&filter).expect("list");
        assert!(page.items.len() <= limit as usize);
        out.extend(ids(&page));
        match page.next_cursor {
            Some(cursor) => filter = filter.after(cursor),
            None => return out,
        }
    }
}

#[test]
fn listing_defaults_to_newest_first_and_pages_stably() {
    let h = Harness::new();
    let mut created: Vec<JobId> = (0..25)
        .map(|i| {
            h.advance(Duration::from_millis(1));
            h.enqueue(JobSpec::new("q", format!("k{i}")))
        })
        .collect();
    let asc = page_through(&h, JobFilter::new().order(SortOrder::Asc), 7);
    assert_eq!(asc, created);
    created.reverse();
    assert_eq!(page_through(&h, JobFilter::new(), 10), created);
    // Exactly a full last page: no dangling cursor.
    let page = h.engine.list(&JobFilter::new().limit(25)).expect("list");
    assert_eq!((page.items.len(), page.next_cursor), (25, None));
}

#[test]
fn paging_continues_correctly_while_jobs_come_and_go() {
    let h = Harness::new();
    let first: Vec<JobId> = (0..6).map(|_| h.enqueue(JobSpec::new("q", "k"))).collect();
    let filter = JobFilter::new().order(SortOrder::Asc).limit(3);
    let page = h.engine.list(&filter).expect("list");
    assert_eq!(ids(&page), first[..3]);
    // A job on the next page is removed and new ones arrive.
    h.engine.cancel(&first[3]).expect("cancel");
    h.engine.delete(&first[3]).expect("delete");
    let later = h.enqueue(JobSpec::new("q", "k"));
    let rest = page_through(&h, filter.after(page.next_cursor.expect("more")), 3);
    assert_eq!(rest, [first[4], first[5], later]);
}

#[test]
fn filters_combine_index_and_document_criteria() {
    let h = Harness::new();
    let a = h.enqueue(
        JobSpec::new("mail", "send")
            .tag("vip")
            .tenant(tenant("acme")),
    );
    h.advance(Duration::from_secs(10));
    let b = h.enqueue(JobSpec::new("mail", "send").tenant(tenant("globex")));
    let c = h.enqueue(JobSpec::new("mail", "render").tag("vip"));
    let d = h.enqueue(JobSpec::new("sms", "send").tag("vip"));
    h.engine.cancel(&c).expect("cancel");
    let all = |f: JobFilter| page_through(&h, f.order(SortOrder::Asc), 2);

    assert_eq!(all(JobFilter::new().queue(q("mail"))), [a, b, c]);
    assert_eq!(all(JobFilter::new().kind("send")), [a, b, d]);
    assert_eq!(all(JobFilter::new().tag("vip")), [a, c, d]);
    assert_eq!(all(JobFilter::new().tenant(tenant("acme"))), [a]);
    assert_eq!(all(JobFilter::new().state(JobState::Cancelled)), [c]);
    assert_eq!(
        all(JobFilter::new()
            .state(JobState::Ready)
            .state(JobState::Cancelled)
            .queue(q("mail"))),
        [a, b, c]
    );
    assert_eq!(
        all(JobFilter::new().created_between(Some(start()), None)),
        [b, c, d],
        "created_after is exclusive"
    );
    assert_eq!(
        all(JobFilter::new().queue(q("mail")).tag("vip").kind("send")),
        [a]
    );
    assert!(all(JobFilter::new().queue(q("nowhere"))).is_empty());
}

#[test]
fn a_rare_match_is_found_behind_many_non_matching_jobs() {
    let h = Harness::new();
    let needle = h.enqueue(JobSpec::new("q", "needle"));
    for _ in 0..5000 {
        h.enqueue(JobSpec::new("q", "hay"));
    }
    let page = h
        .engine
        .list(&JobFilter::new().kind("needle"))
        .expect("list");
    assert_eq!(ids(&page), [needle]);
    assert!(page.next_cursor.is_none());
}

#[test]
fn a_cursor_from_another_listing_is_rejected() {
    let h = Harness::new();
    let filter = JobFilter::new().after(Cursor::from_name("emails"));
    let err = h.engine.list(&filter).unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidArgument);
    assert_eq!(err.to_string(), "$.cursor: was not issued by a job listing");
}

#[test]
fn queue_info_reports_counts_age_and_throughput() {
    let h = Harness::new();
    for _ in 0..3 {
        h.enqueue(JobSpec::new("q", "k"));
    }
    h.enqueue(JobSpec::new("q", "later").delay(Duration::from_secs(600)));
    let l = h.lease_one("q");
    h.complete(&l);
    let l = h.lease_one("q");
    h.fail(&l, false);
    h.advance(Duration::from_secs(40));
    let info = h.engine.queue_info(&q("q")).expect("info");
    assert_eq!(
        (
            info.counts.ready,
            info.counts.scheduled,
            info.counts.succeeded,
            info.counts.dead
        ),
        (1, 1, 1, 1)
    );
    assert_eq!(info.oldest_ready_age, Some(Duration::from_secs(40)));
    assert_eq!(
        (
            info.throughput.enqueued,
            info.throughput.succeeded,
            info.throughput.failed,
            info.throughput.dead
        ),
        (4, 1, 1, 1)
    );
    h.advance(Duration::from_secs(60));
    let info = h.engine.queue_info(&q("q")).expect("info");
    assert_eq!(info.throughput.enqueued, 0, "outside the one-minute window");
    assert!(matches!(
        h.engine.queue_info(&q("missing")),
        Err(Error::QueueNotFound(_))
    ));
}

#[test]
fn queues_are_listed_by_name() {
    let h = Harness::new();
    for name in ["zeta", "alpha", "mid"] {
        h.enqueue(JobSpec::new(q(name), "k"));
    }
    let names: Vec<String> = h
        .engine
        .list_queues()
        .into_iter()
        .map(|i| i.name.to_string())
        .collect();
    assert_eq!(names, ["alpha", "mid", "zeta"]);
}

#[test]
fn stats_add_up_across_queues() {
    let h = Harness::new();
    let mut config = QueueConfig::default();
    config.rate_limit = Some(RateLimit::new(10.0, 5));
    h.engine.set_config(&q("limited"), config).expect("config");
    h.enqueue(JobSpec::new("limited", "k"));
    h.enqueue(JobSpec::new("free", "k").tenant(tenant("t")));
    h.enqueue(JobSpec::new("free", "k"));
    let l = h.lease_one("limited");
    h.complete(&l);
    let stats = h.engine.stats();
    assert_eq!(stats.totals.total(), 3);
    assert_eq!(stats.totals.ready, 2);
    let limited = stats.queue("limited").expect("limited");
    assert!(limited.rate_tokens.is_some_and(|t| (t - 4.0).abs() < 1e-6));
    let free = stats.queue("free").expect("free");
    assert_eq!((free.tenants, free.rate_tokens), (2, None));
    assert_eq!(
        (
            stats.counters.enqueued,
            stats.counters.leased,
            stats.counters.succeeded
        ),
        (3, 1, 1)
    );
    assert_eq!(stats.counters.lease_requests, 1);
    assert_eq!(stats.workers, 1);
}

#[test]
fn lease_latency_is_measured_from_eligibility() {
    let h = Harness::with_options(EngineOptions::new().latency_samples(2));
    h.enqueue(JobSpec::new("q", "a"));
    h.enqueue(JobSpec::new("q", "b").delay(Duration::from_secs(5)));
    h.advance(Duration::from_secs(8));
    let leased = h.lease("q", 2);
    assert_eq!(leased.len(), 2);
    let mut waits: Vec<Duration> = h
        .engine
        .drain_lease_latencies()
        .into_iter()
        .map(|s| s.waited)
        .collect();
    waits.sort();
    assert_eq!(waits, [Duration::from_secs(3), Duration::from_secs(8)]);
    assert!(h.engine.drain_lease_latencies().is_empty());
    let summary = h.engine.stats().lease_latency;
    assert_eq!((summary.count, summary.max), (2, Duration::from_secs(8)));
    // Undrained samples beyond the capacity are dropped and counted.
    for _ in 0..3 {
        h.enqueue(JobSpec::new("q", "c"));
    }
    h.lease("q", 3);
    assert_eq!(h.engine.drain_lease_latencies().len(), 2);
    assert_eq!(h.engine.stats().latency_samples_dropped, 1);
}

#[test]
fn workers_report_their_queues_and_outcomes() {
    let h = Harness::new();
    for _ in 0..3 {
        h.enqueue(JobSpec::new("q", "k"));
    }
    let leased = h.lease_from("w1", &["q", "other"], 3);
    h.complete(&leased[0]);
    h.fail(&leased[1], true);
    h.advance(Duration::from_secs(1));
    h.heartbeat(&leased[2]);
    let workers = h.engine.workers();
    assert_eq!(workers.len(), 1);
    let info = &workers[0];
    assert_eq!(info.queues, [q("q"), q("other")]);
    assert_eq!(
        (info.leased, info.succeeded, info.failed, info.active_jobs),
        (3, 1, 1, 1)
    );
    assert_eq!((info.first_seen, info.last_seen), (start(), h.now()));
}

#[test]
fn invalid_specs_are_rejected_with_every_problem() {
    let h = Harness::new();
    let mut spec = JobSpec::new("q", "bad kind").priority(5000);
    spec.max_attempts = Some(0);
    let err = h.engine.enqueue(spec).unwrap_err();
    let Error::Invalid(violations) = &err else {
        panic!("expected invalid, got {err}");
    };
    assert_eq!(violations.len(), 3);
    assert!(h.engine.list_queues().is_empty(), "nothing created");
    let api = ApiError::from(err);
    assert_eq!(api.code, ErrorCode::InvalidArgument);
}

#[test]
fn batch_validation_names_the_offending_spec() {
    let h = Harness::new();
    let err = h
        .engine
        .enqueue_batch(vec![
            JobSpec::new("q", "fine"),
            JobSpec::new("q", "fine").priority(2000),
        ])
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "$.jobs[1].priority: must be between -1000 and 1000, got 2000"
    );
    assert!(h.all_jobs().is_empty());
    assert!(h
        .engine
        .enqueue_batch(Vec::new())
        .expect("empty")
        .is_empty());
    let too_many = vec![JobSpec::new("q", "k"); belay_proto::limits::MAX_ENQUEUE_BATCH + 1];
    assert_eq!(
        h.engine.enqueue_batch(too_many).unwrap_err().code(),
        ErrorCode::InvalidArgument
    );
}

#[test]
fn lease_requests_are_validated() {
    let h = Harness::new();
    let req = belay_proto::api::LeaseRequest::new(w("w1"), Vec::new());
    let err = h.engine.lease(&req).unwrap_err();
    assert_eq!(err.to_string(), "$.queues: must name at least one queue");
    assert!(
        h.engine.workers().is_empty(),
        "an invalid request is not a sighting"
    );
}

#[test]
fn invalid_configs_change_nothing() {
    let h = Harness::new();
    let mut config = QueueConfig::default();
    config.max_concurrency = Some(0);
    let err = h.engine.set_config(&q("q"), config).unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidArgument);
    assert!(h.engine.queue_info(&q("q")).is_err());
    let mut config = QueueConfig::default();
    config.max_concurrency = Some(4);
    let info = h
        .engine
        .set_config(&q("q"), config.clone())
        .expect("config");
    assert_eq!(info.config, config);
    assert_eq!(h.events(), ["queue.configured"]);
}

#[test]
fn deleting_a_queue_requires_it_to_be_drained_unless_forced() {
    let h = Harness::with_options(EngineOptions::new().batch_size(3));
    h.enqueue(
        JobSpec::new("q", "done")
            .unique("u", belay_proto::UniqueScope::For(Duration::from_secs(600))),
    );
    let l = h.lease_one("q");
    h.complete(&l);
    let pending: Vec<JobId> = (0..7).map(|_| h.enqueue(JobSpec::new("q", "k"))).collect();
    let running = h.lease_one("q");
    let err = h.engine.delete_queue(&q("q"), false).unwrap_err();
    assert_eq!(
        err.to_string(),
        "queue q has 7 pending jobs; cancel or purge them, or delete the queue with force"
    );
    assert_eq!(h.engine.delete_queue(&q("q"), true).expect("forced"), 8);
    assert!(h.engine.queue_info(&q("q")).is_err());
    assert!(h.engine.get(&pending[0]).is_err());
    // The worker holding a job of the deleted queue loses it.
    let err = h
        .engine
        .complete(&running.job.id, &CompleteRequest::new(running.token))
        .unwrap_err();
    assert!(matches!(err, Error::LeaseLost(_)));
    // Timed claims went with the queue.
    let fresh = h
        .engine
        .enqueue(
            JobSpec::new("q", "done")
                .unique("u", belay_proto::UniqueScope::For(Duration::from_secs(600))),
        )
        .expect("enqueue");
    assert_eq!(fresh.status, belay_proto::api::EnqueueStatus::Created);
    assert!(h.events().contains(&"queue.deleted"));
    assert_eq!(h.engine.workers()[0].active_jobs, 0);
    h.assert_consistent();
}

#[test]
fn a_queue_with_only_finished_jobs_can_be_deleted() {
    let h = Harness::new();
    let id = h.enqueue(JobSpec::new("q", "k"));
    h.engine.cancel(&id).expect("cancel");
    assert_eq!(h.engine.delete_queue(&q("q"), false).expect("delete"), 1);
    assert!(h.engine.list_queues().is_empty());
    assert!(matches!(
        h.engine.delete_queue(&q("q"), false),
        Err(Error::QueueNotFound(_))
    ));
}

#[test]
fn job_state_is_answered_from_memory() {
    let h = Harness::new();
    let id = h.enqueue(JobSpec::new("q", "k"));
    assert_eq!(h.engine.job_state(&id), Some(JobState::Ready));
    h.engine.cancel(&id).expect("cancel");
    assert_eq!(h.engine.job_state(&id), Some(JobState::Cancelled));
    assert_eq!(h.engine.job_state(&JobId::generate()), None);
}

#[test]
fn the_engine_serves_workers_through_the_job_source_trait() {
    use belay_proto::source::JobSource;
    let h = Harness::new();
    let id = h.enqueue(JobSpec::new("q", "k"));
    let source: std::sync::Arc<dyn JobSource> = h.engine.clone();
    let req = belay_proto::api::LeaseRequest::new(w("w1"), vec![q("q")]);
    let leased = source.lease(&req).expect("lease");
    let token = leased[0].token;
    let err = source
        .complete(&id, &CompleteRequest::new(belay_proto::LeaseToken::new(1)))
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::LeaseLost);
    source
        .complete(&id, &CompleteRequest::new(token))
        .expect("complete");
    assert_eq!(h.state(id), JobState::Succeeded);
}

#[test]
fn the_engine_is_shareable_between_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<belay_queue::QueueEngine>();
}
