//! What an operator sees and does over the API: paging through jobs,
//! queue defaults, retention and deletion of finished jobs, workers that
//! went silent, metrics, and shutting the server down while workers are
//! long-polling.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use belay_proto::api::SortOrder;
use belay_proto::{Retention, WorkerId};
use belay_testkit::prelude::*;
use belay_testkit::raw;

#[test]
fn listings_page_through_every_job_in_a_stable_order() -> Result<()> {
    let server = TestServer::start()?;
    let client = server.client();
    let queue = QueueName::from_static("pages");
    let mut enqueued = Vec::new();
    for chunk in 0..5 {
        let specs = numbered_jobs(&queue, "item", 50);
        enqueued.extend(
            client
                .jobs()
                .enqueue_batch(&specs)?
                .into_iter()
                .map(|r| r.id),
        );
        assert_eq!(enqueued.len(), (chunk + 1) * 50);
    }

    // Oldest first, a hundred at a time.
    let mut pages = 0;
    let mut seen = Vec::new();
    let mut filter = JobFilter::new()
        .queue(queue.clone())
        .order(SortOrder::Asc)
        .limit(100);
    loop {
        let page = client.jobs().list(&filter)?;
        pages += 1;
        seen.extend(page.items.iter().map(|j| j.id));
        match page.next_cursor {
            Some(cursor) => filter = filter.after(cursor),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    assert_eq!(seen, enqueued);

    // Newest first by default; the lazy listing fetches every page.
    let newest_first: Vec<JobId> = client
        .jobs()
        .list_all(JobFilter::new().queue(queue).limit(64))
        .map(|j| j.map(|j| j.id))
        .collect::<std::result::Result<_, _>>()?;
    let mut expected = enqueued.clone();
    expected.reverse();
    assert_eq!(newest_first, expected);
    let unique: BTreeSet<JobId> = newest_first.into_iter().collect();
    assert_eq!(unique.len(), 250);
    Ok(())
}

#[test]
fn queue_defaults_apply_to_jobs_that_do_not_choose_for_themselves() -> Result<()> {
    let server = TestServer::start()?;
    let client = server.client();
    let config = QueueConfig {
        default_max_attempts: 2,
        default_retry: belay_common::BackoffPolicy::constant(Duration::from_secs(7)),
        ..QueueConfig::default()
    };
    let info = client
        .queues()
        .set_config(&QueueName::from_static("flaky"), &config)?;
    assert_eq!(info.config.default_max_attempts, 2);

    let worker = server
        .worker()
        .queue("flaky")
        .failing_n_times("call", 5)
        .start()?;
    // A bare spec: no retry policy, no attempt budget of its own.
    let id = client.jobs().enqueue(&JobSpec::new("flaky", "call"))?.id;
    let t0 = server.now();
    let waiting = wait_for_state(&client, id, JobState::Retrying, DEFAULT_TIMEOUT)?;
    assert_eq!(waiting.run_at, t0 + Duration::from_secs(7));
    assert_eq!(waiting.spec.max_attempts, Some(2));
    server.tick(Duration::from_secs(7))?;
    let dead = wait_for_state(&client, id, JobState::Dead, DEFAULT_TIMEOUT)?;
    assert_eq!(dead.attempt, 2);
    assert_eq!(worker.recorder().attempts_of(id), [1, 2]);
    Ok(())
}

#[test]
fn finished_jobs_are_deleted_after_their_retention_or_on_request() -> Result<()> {
    let server = TestServer::start()?;
    let events = server.event_log()?;
    let client = server.client();
    let config = QueueConfig {
        retention: Retention {
            succeeded: Duration::from_secs(3600),
            ..Retention::default()
        },
        ..QueueConfig::default()
    };
    client
        .queues()
        .set_config(&QueueName::from_static("short-lived"), &config)?;
    let _worker = server
        .worker()
        .queue("short-lived")
        .recording("tmp")
        .start()?;
    let kept = client.jobs().enqueue(&job("short-lived", "tmp"))?.id;
    let removed = client.jobs().enqueue(&job("short-lived", "tmp"))?.id;
    wait_for_state(&client, kept, JobState::Succeeded, DEFAULT_TIMEOUT)?;
    wait_for_state(&client, removed, JobState::Succeeded, DEFAULT_TIMEOUT)?;

    // An operator deletes one at once.
    client.jobs().delete(removed)?;
    assert!(client
        .jobs()
        .get(removed)
        .expect_err("deleted")
        .is_not_found());

    // Retention takes the other an hour after it finished, not before.
    server.tick(Duration::from_secs(3599))?;
    assert_eq!(client.jobs().get(kept)?.state, JobState::Succeeded);
    let report = server.tick(Duration::from_secs(1))?;
    assert_eq!(report.queue.deleted, 1);
    assert!(client.jobs().get(kept).expect_err("expired").is_not_found());
    events.wait_for(DEFAULT_TIMEOUT, |events| {
        events
            .iter()
            .filter(|e| e.type_name() == "job.deleted")
            .count()
            == 2
    })?;

    // Unfinished jobs cannot be deleted.
    let pending = client
        .jobs()
        .enqueue(&job("short-lived", "tmp").delay(Duration::from_secs(60)))?
        .id;
    let refused = client.jobs().delete(pending).expect_err("still waiting");
    assert_eq!(refused.code(), Some(ErrorCode::FailedPrecondition));
    Ok(())
}

#[test]
fn a_worker_that_goes_silent_is_reported_lost() -> Result<()> {
    let server = TestServer::start()?;
    let events = server.event_log()?;
    let worker = server
        .worker()
        .id("night-shift")
        .queue("batch")
        .recording("run")
        .start()?;
    let client = server.client();
    let id = client.jobs().enqueue(&job("batch", "run"))?.id;
    wait_for_state(&client, id, JobState::Succeeded, DEFAULT_TIMEOUT)?;
    let known = client.workers()?;
    let seen = known
        .iter()
        .find(|w| w.id.as_str() == "night-shift")
        .expect("the worker is known after leasing");
    assert_eq!(seen.succeeded, 1);
    assert_eq!(seen.queues, [QueueName::from_static("batch")]);

    let _ = worker.crash();
    // Default worker timeout: two minutes of silence.
    server.tick(Duration::from_secs(119))?;
    assert_eq!(events.count("worker.lost"), 0);
    let report = server.tick(Duration::from_secs(2))?;
    assert_eq!(report.queue.workers_lost, 1);
    let lost = events.wait_for_event(DEFAULT_TIMEOUT, |e| e.type_name() == "worker.lost")?;
    assert_eq!(
        lost.kind.worker(),
        Some(&WorkerId::from_static("night-shift"))
    );
    Ok(())
}

#[test]
fn metrics_count_job_outcomes_by_queue() -> Result<()> {
    let server = TestServer::start()?;
    let _worker = server
        .worker()
        .queue("metered")
        .recording("ok")
        .failing("broken")
        .start()?;
    let client = server.client();
    let ids = [
        client.jobs().enqueue(&job("metered", "ok"))?.id,
        client.jobs().enqueue(&job("metered", "ok"))?.id,
        client.jobs().enqueue(&job("metered", "broken"))?.id,
    ];
    for id in ids {
        wait_for_terminal(&client, id, DEFAULT_TIMEOUT)?;
    }
    let text = client.metrics()?;
    for line in [
        "belay_jobs_enqueued_total{queue=\"metered\"} 3",
        "belay_jobs_started_total{queue=\"metered\"} 3",
        "belay_jobs_succeeded_total{queue=\"metered\"} 2",
        "belay_jobs_dead_total{queue=\"metered\"} 1",
    ] {
        assert!(
            text.lines().any(|l| l == line),
            "missing {line:?} in:\n{text}"
        );
    }
    Ok(())
}
