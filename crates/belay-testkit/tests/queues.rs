//! Queue controls seen from workers: pausing and resuming, a token-bucket
//! rate limit driven by the manual clock, a concurrency limit enforced
//! across several workers, and purging.
//!
//! "Nothing more runs" is never checked by sleeping: the tests wait until
//! workers with free slots have asked for work and been refused a few
//! times, which proves the server held the jobs back.

use std::time::Duration;

use belay_proto::RateLimit;
use belay_testkit::prelude::*;

fn queue(name: &'static str) -> QueueName {
    QueueName::from_static(name)
}

#[test]
fn a_paused_queue_holds_its_jobs_until_resumed_while_others_keep_flowing() -> Result<()> {
    let server = TestServer::start()?;
    let events = server.event_log()?;
    let worker = server
        .worker()
        .queues(["mail", "sms"])
        .recording("send")
        .start()?;
    let client = server.client();
    client.queues().pause(&queue("mail"))?;

    let mail: Vec<JobId> = (0..3)
        .map(|n| {
            Ok(client
                .jobs()
                .enqueue(&job("mail", "send").payload(numbered(n)))?
                .id)
        })
        .collect::<Result<_>>()?;
    let sms = client.jobs().enqueue(&job("sms", "send"))?.id;

    // The other queue is served while this one is held.
    wait_for_state(&client, sms, JobState::Succeeded, DEFAULT_TIMEOUT)?;
    worker.wait_for_empty_leases(3, DEFAULT_TIMEOUT)?;
    for id in &mail {
        assert_eq!(client.jobs().get(*id)?.state, JobState::Ready);
    }
    let info = client.queues().get(&queue("mail"))?;
    assert!(info.paused);
    assert_eq!(info.counts.get(JobState::Ready), 3);

    client.queues().resume(&queue("mail"))?;
    for id in &mail {
        wait_for_state(&client, *id, JobState::Succeeded, DEFAULT_TIMEOUT)?;
    }
    assert!(!client.queues().get(&queue("mail"))?.paused);
    events.wait_for(DEFAULT_TIMEOUT, |events| {
        events.iter().any(|e| e.type_name() == "queue.resumed")
    })?;
    let queue_events: Vec<&str> = events
        .events()
        .iter()
        .filter(|e| e.kind.category() == "queue")
        .map(|e| e.type_name())
        .collect();
    assert_eq!(queue_events, ["queue.paused", "queue.resumed"]);
    Ok(())
}

#[test]
fn a_rate_limited_queue_hands_out_jobs_at_the_configured_pace() -> Result<()> {
    let server = TestServer::start()?;
    let client = server.client();
    let config = QueueConfig {
        rate_limit: Some(RateLimit::new(1.0, 3)),
        ..QueueConfig::default()
    };
    client.queues().set_config(&queue("partner-api"), &config)?;
    for n in 0..8 {
        client
            .jobs()
            .enqueue(&job("partner-api", "call").payload(numbered(n)))?;
    }
    let worker = server
        .worker()
        .queue("partner-api")
        .concurrency(4)
        .recording("call")
        .start()?;
    let recorder = worker.recorder();

    // A full bucket: the burst goes out at once, then nothing.
    recorder.wait_until(DEFAULT_TIMEOUT, |r| r.succeeded() == 3)?;
    worker.wait_for_empty_leases(3, DEFAULT_TIMEOUT)?;
    assert_eq!(recorder.len(), 3);

    // One token per second of (manual) time.
    server.tick(Duration::from_secs(1))?;
    recorder.wait_until(DEFAULT_TIMEOUT, |r| r.succeeded() == 4)?;
    worker.wait_for_empty_leases(3, DEFAULT_TIMEOUT)?;
    assert_eq!(recorder.len(), 4);

    server.tick(Duration::from_secs(2))?;
    recorder.wait_until(DEFAULT_TIMEOUT, |r| r.succeeded() == 6)?;
    worker.wait_for_empty_leases(3, DEFAULT_TIMEOUT)?;
    assert_eq!(recorder.len(), 6);

    // A long pause refills to the burst, which covers the last two.
    server.tick(Duration::from_secs(60))?;
    recorder.wait_until(DEFAULT_TIMEOUT, |r| r.succeeded() == 8)?;
    // The recorder sees a handler return before the worker reports it.
    let info = eventually_some(DEFAULT_TIMEOUT, "all eight reported", || {
        client
            .queues()
            .get(&queue("partner-api"))
            .ok()
            .filter(|info| info.counts.get(JobState::Succeeded) == 8)
    })?;
    assert_eq!(info.config.rate_limit, Some(RateLimit::new(1.0, 3)));
    assert_eq!(recorder.len(), 8);
    Ok(())
}

#[test]
fn a_concurrency_limit_holds_across_several_workers() -> Result<()> {
    const JOBS: usize = 9;
    let server = TestServer::start()?;
    let client = server.client();
    let config = QueueConfig {
        max_concurrency: Some(2),
        ..QueueConfig::default()
    };
    client.queues().set_config(&queue("render"), &config)?;
    let ids: Vec<JobId> = (0..JOBS)
        .map(|n| {
            Ok(client
                .jobs()
                .enqueue(&job("render", "frame").payload(numbered(n)))?
                .id)
        })
        .collect::<Result<_>>()?;

    let recorder = Recorder::new();
    let gate = Gate::new();
    let workers: Vec<TestWorker> = (0..3)
        .map(|_| {
            server
                .worker()
                .queue("render")
                .concurrency(3)
                .recorder(&recorder)
                .gated("frame", &gate)
                .start()
        })
        .collect::<Result<_>>()?;

    recorder.wait_until(DEFAULT_TIMEOUT, |r| r.running_in("render") == 2)?;
    // Seven free slots between them, and every worker is turned away.
    for worker in &workers {
        worker.wait_for_empty_leases(3, DEFAULT_TIMEOUT)?;
    }
    assert_eq!(recorder.running_in("render"), 2);
    let info = client.queues().get(&queue("render"))?;
    assert_eq!(info.counts.get(JobState::Running), 2);
    assert_eq!(info.counts.get(JobState::Ready), (JOBS - 2) as u64);

    gate.open();
    for id in &ids {
        wait_for_state(&client, *id, JobState::Succeeded, DEFAULT_TIMEOUT)?;
    }
    assert_eq!(recorder.peak_running("render"), 2);
    assert_eq!(recorder.succeeded(), JOBS);
    assert!(recorder.overlaps().is_empty());
    Ok(())
}
