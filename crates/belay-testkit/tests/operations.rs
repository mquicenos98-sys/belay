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
