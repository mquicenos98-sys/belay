//! Jobs from enqueue to a final state through the HTTP API and the worker
//! runtime: results in `GET` and in the event stream, uniqueness under
//! concurrent enqueues, priorities, delays, progress, cancellation of a
//! running job, and a lease left behind by a worker that vanished.

use std::collections::BTreeMap;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use belay_proto::api::{EnqueueResult, EnqueueStatus};
use belay_proto::{OnConflict, Unique};
use belay_testkit::fixtures::number_of;
use belay_testkit::prelude::*;
use belay_worker::{handler_fn, HandlerError, Outcome};

#[test]
fn a_job_runs_on_a_worker_and_its_result_shows_over_http_and_in_the_event_stream() -> Result<()> {
    let server = TestServer::start()?;
    let events = server.event_log()?;
    let worker = server
        .worker()
        .queue("emails")
        .recording("send_welcome")
        .start()?;
    let client = server.client();

    let payload = json!({"user": 42, "template": "welcome"});
    let created = client
        .jobs()
        .enqueue(&job("emails", "send_welcome").payload(payload.clone()))?;
    assert_eq!(created.status, EnqueueStatus::Created);
    assert_eq!(created.state, JobState::Ready);

    let done = wait_for_state(&client, created.id, JobState::Succeeded, DEFAULT_TIMEOUT)?;
    assert_eq!(done.result, Some(payload.clone()));
    assert_eq!(done.attempt, 1);
    assert!(done.errors.is_empty());
    assert!(done.lease.is_none(), "finished jobs hold no lease");
    assert_eq!(done.finished_at, Some(server.now()));

    let runs = worker.recorder().of_job(created.id);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].payload, payload);
    assert_eq!(runs[0].worker, *worker.id());

    events.wait_for_job_event(created.id, "job.succeeded", DEFAULT_TIMEOUT)?;
    assert_eq!(
        events.types_of_job(created.id),
        ["job.enqueued", "job.started", "job.succeeded"]
    );
    let started = events.wait_for_job_event(created.id, "job.started", DEFAULT_TIMEOUT)?;
    match started.kind {
        EventKind::JobStarted {
            worker: by,
            attempt,
            ..
        } => assert_eq!((by, attempt), (worker.id().clone(), 1)),
        other => panic!("expected job.started, got {other:?}"),
    }

    let succeeded = client.jobs().list(
        &JobFilter::new()
            .queue(QueueName::from_static("emails"))
            .state(JobState::Succeeded),
    )?;
    assert_eq!(succeeded.items.len(), 1);
    assert_eq!(succeeded.items[0].id, created.id);
    assert!(client.workers()?.iter().any(|w| w.id == *worker.id()));
    Ok(())
}

#[test]
fn a_unique_key_admits_one_job_however_many_threads_race_for_it() -> Result<()> {
    const THREADS: usize = 16;
    const KEYS: usize = 4;
    let server = TestServer::start()?;
    let barrier = Arc::new(Barrier::new(THREADS));
    let racers: Vec<_> = (0..THREADS)
        .map(|t| {
            let (barrier, url) = (Arc::clone(&barrier), server.url());
            thread::spawn(move || -> Result<Vec<EnqueueResult>> {
                let client = belay_testkit::connect(&url, None)?;
                barrier.wait();
                (0..KEYS)
                    .map(|k| {
                        let spec = job("orders", "charge")
                            .payload(json!({"thread": t}))
                            .unique(format!("order:{k}"), UniqueScope::UntilFinished);
                        Ok(client.jobs().enqueue(&spec)?)
                    })
                    .collect()
            })
        })
        .collect();

    let mut by_key: BTreeMap<usize, Vec<EnqueueResult>> = BTreeMap::new();
    for racer in racers {
        let results = racer.join().expect("an enqueueing thread panicked")?;
        for (k, result) in results.into_iter().enumerate() {
            by_key.entry(k).or_default().push(result);
        }
    }
    for (k, results) in &by_key {
        let created = results
            .iter()
            .filter(|r| r.status == EnqueueStatus::Created)
            .count();
        assert_eq!(created, 1, "order:{k}: {results:?}");
        let ids: Vec<JobId> = results.iter().map(|r| r.id).collect();
        assert!(
            ids.windows(2).all(|w| w[0] == w[1]),
            "every racer for order:{k} got the same job: {ids:?}"
        );
    }
    let client = server.client();
    let stored = client
        .jobs()
        .list(&JobFilter::new().queue(QueueName::from_static("orders")))?;
    assert_eq!(stored.items.len(), KEYS);
    Ok(())
}

#[test]
fn a_rejecting_unique_key_fails_every_racer_but_one() -> Result<()> {
    const THREADS: usize = 12;
    let server = TestServer::start()?;
    let barrier = Arc::new(Barrier::new(THREADS));
    let racers: Vec<_> = (0..THREADS)
        .map(|_| {
            let (barrier, url) = (Arc::clone(&barrier), server.url());
            thread::spawn(move || -> Result<std::result::Result<JobId, ErrorCode>> {
                let client = belay_testkit::connect(&url, None)?;
                let unique = Unique::new("refund:9", UniqueScope::UntilFinished)
                    .on_conflict(OnConflict::Reject);
                barrier.wait();
                Ok(
                    match client
                        .jobs()
                        .enqueue(&job("refunds", "refund").unique_with(unique))
                    {
                        Ok(created) => Ok(created.id),
                        Err(e) => Err(e.code().expect("refusals are API errors")),
                    },
                )
            })
        })
        .collect();
    let outcomes: Vec<_> = racers
        .into_iter()
        .map(|r| r.join().expect("a racing thread panicked"))
        .collect::<Result<_>>()?;
    let winners: Vec<&JobId> = outcomes.iter().filter_map(|o| o.as_ref().ok()).collect();
    assert_eq!(winners.len(), 1, "{outcomes:?}");
    assert!(outcomes
        .iter()
        .filter_map(|o| o.as_ref().err())
        .all(|code| *code == ErrorCode::AlreadyExists));
    Ok(())
}

#[test]
fn higher_priorities_run_first_and_ties_keep_their_enqueue_order() -> Result<()> {
    let server = TestServer::start()?;
    let client = server.client();
    let priorities: [i16; 8] = [0, 5, -3, 10, 5, 1, -3, 7];
    for (n, priority) in priorities.iter().enumerate() {
        let spec = job("ranked", "rank")
            .priority(*priority)
            .payload(numbered(n));
        client.jobs().enqueue(&spec)?;
    }
    // One executor slot: the worker leases one job at a time, so the order
    // of executions is the order the server handed them out.
    let worker = server
        .worker()
        .queue("ranked")
        .concurrency(1)
        .recording("rank")
        .start()?;
    worker
        .recorder()
        .wait_until(DEFAULT_TIMEOUT, |r| r.succeeded() == priorities.len())?;
    let order: Vec<u64> = worker
        .recorder()
        .executions()
        .iter()
        .filter_map(|e| number_of(&e.payload))
        .collect();
    assert_eq!(order, [3, 7, 1, 4, 5, 0, 2, 6]);
    Ok(())
}

#[test]
fn delayed_jobs_wait_for_the_clock_and_run_once_a_tick_promotes_them() -> Result<()> {
    let server = TestServer::start()?;
    let worker = server
        .worker()
        .queue("reminders")
        .recording("remind")
        .start()?;
    let client = server.client();
    let start = server.now();

    let later = client.jobs().enqueue(
        &job("reminders", "remind")
            .delay(Duration::from_secs(90))
            .payload(numbered(90)),
    )?;
    let sooner = client.jobs().enqueue(
        &job("reminders", "remind")
            .run_at(start + Duration::from_secs(30))
            .payload(numbered(30)),
    )?;
    assert_eq!(later.state, JobState::Scheduled);
    assert_eq!(sooner.state, JobState::Scheduled);

    // The worker keeps asking and keeps being told there is nothing due.
    worker.wait_for_empty_leases(3, DEFAULT_TIMEOUT)?;
    assert!(worker.recorder().is_empty());

    let report = server.tick(Duration::from_secs(30))?;
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let first = wait_for_state(&client, sooner.id, JobState::Succeeded, DEFAULT_TIMEOUT)?;
    assert_eq!(first.started_at, Some(start + Duration::from_secs(30)));
    worker.wait_for_empty_leases(2, DEFAULT_TIMEOUT)?;
    assert_eq!(client.jobs().get(later.id)?.state, JobState::Scheduled);

    server.tick(Duration::from_secs(60))?;
    let second = wait_for_state(&client, later.id, JobState::Succeeded, DEFAULT_TIMEOUT)?;
    assert_eq!(second.run_at, start + Duration::from_secs(90));
    assert_eq!(second.started_at, Some(start + Duration::from_secs(90)));
    let ran: Vec<u64> = worker
        .recorder()
        .executions()
        .iter()
        .filter_map(|e| number_of(&e.payload))
        .collect();
    assert_eq!(ran, [30, 90]);
    Ok(())
}

#[test]
fn an_operator_can_promote_a_delayed_job_to_run_now() -> Result<()> {
    let server = TestServer::start()?;
    let worker = server.worker().queue("digest").recording("send").start()?;
    let client = server.client();
    let id = client
        .jobs()
        .enqueue(&job("digest", "send").delay(Duration::from_secs(3600)))?
        .id;
    worker.wait_for_empty_leases(2, DEFAULT_TIMEOUT)?;
    let promoted = client.jobs().promote(id)?;
    assert_eq!(promoted.state, JobState::Ready);
    let done = wait_for_state(&client, id, JobState::Succeeded, DEFAULT_TIMEOUT)?;
    assert_eq!(done.started_at, Some(server.now()));
    Ok(())
}

#[test]
fn progress_reported_by_a_handler_travels_with_its_heartbeats() -> Result<()> {
    let server = TestServer::start()?;
    let gate = Gate::new();
    let held = gate.clone();
    let worker = server
        .worker()
        .queue("video")
        .handler(
            "encode",
            handler_fn(move |ctx| {
                ctx.progress(40.0, "pass 1 of 2");
                while !held.is_open() {
                    if !ctx.sleep_or_cancel(Duration::from_millis(5)) {
                        return Err(HandlerError::cancelled());
                    }
                }
                Ok(Outcome::with(json!({"frames": 1200})))
            }),
        )
        .start()?;
    let client = server.client();
    let id = client.jobs().enqueue(&job("video", "encode"))?.id;
    let running = wait_for_job(&client, id, "running with progress", DEFAULT_TIMEOUT, |j| {
        j.progress.is_some()
    })?;
    let progress = running.progress.expect("the wait checked it");
    assert_eq!(progress.percent, Some(40.0));
    assert_eq!(progress.message.as_deref(), Some("pass 1 of 2"));
    assert_eq!(running.state, JobState::Running);
    gate.open();
    let done = wait_for_state(&client, id, JobState::Succeeded, DEFAULT_TIMEOUT)?;
    assert_eq!(done.result, Some(json!({"frames": 1200})));
    drop(worker);
    Ok(())
}

#[test]
fn cancelling_a_running_job_reaches_its_handler_through_the_next_heartbeat() -> Result<()> {
    let server = TestServer::start()?;
    let events = server.event_log()?;
    let worker = server
        .worker()
        .queue("reports")
        .sleeping("render", Duration::from_secs(60))
        .heartbeat_every(Duration::from_millis(50))
        .start()?;
    let client = server.client();
    let id = client.jobs().enqueue(&job("reports", "render"))?.id;
    worker.recorder().wait_for_start(id, DEFAULT_TIMEOUT)?;

    // A running job is only flagged; its worker hears of it on the next
    // heartbeat and cancels the handler, which then reports back.
    let flagged = client.jobs().cancel(id)?;
    assert_eq!(flagged.state, JobState::Running);
    assert!(flagged.cancel_requested);

    let cancelled = wait_for_state(&client, id, JobState::Cancelled, DEFAULT_TIMEOUT)?;
    assert!(cancelled.finished_at.is_some());
    let runs = worker.recorder().of_job(id);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].outcome, Recorded::Cancelled);
    let took = runs[0].took.expect("the attempt has ended");
    assert!(took < Duration::from_secs(30), "the handler ran {took:?}");
    events.wait_for_job_event(id, "job.cancelled", DEFAULT_TIMEOUT)?;
    eventually(DEFAULT_TIMEOUT, || worker.stats().cancelled == 1)?;

    // Cancelling again changes nothing; the attempt count stays at one.
    let again = client.jobs().cancel(id)?;
    assert_eq!((again.state, again.attempt), (JobState::Cancelled, 1));
    Ok(())
}

#[test]
fn a_lease_left_behind_by_a_vanished_worker_expires_and_another_worker_finishes_the_job(
) -> Result<()> {
    let server = TestServer::start()?;
    let events = server.event_log()?;
    let gate = Gate::new();
    let doomed = server
        .worker()
        .id("doomed")
        .queue("imports")
        .gated("import", &gate)
        .lease_for(Duration::from_secs(30))
        .start()?;
    let client = server.client();
    let id = client
        .jobs()
        .enqueue(&job("imports", "import").payload(json!({"file": "a.csv"})))?
        .id;
    doomed.recorder().wait_for_start(id, DEFAULT_TIMEOUT)?;

    // The worker disappears without a word; the server still thinks it
    // is working on the job.
    let report = doomed.crash();
    assert_eq!(report.stats.succeeded, 0);
    let stranded = client.jobs().get(id)?;
    assert_eq!(stranded.state, JobState::Running);
    let lease = stranded.lease.expect("a running job has a lease");
    assert_eq!(lease.worker.as_str(), "doomed");

    // Not yet: the lease has time left.
    server.tick(Duration::from_secs(20))?;
    assert_eq!(client.jobs().get(id)?.state, JobState::Running);
    // Now it has run out; the attempt counts as failed and the job waits
    // out its retry delay (1 s for fixtures::job).
    let tick = server.tick(Duration::from_secs(11))?;
    assert_eq!(tick.queue.expired, 1);
    let expired = client.jobs().get(id)?;
    assert_eq!(expired.state, JobState::Retrying);
    let error = expired.last_error().expect("the expiry is recorded");
    assert!(error.message.contains("lease expired"), "{error:?}");
    assert_eq!(error.worker.as_ref().map(|w| w.as_str()), Some("doomed"));
    events.wait_for_job_event(id, "job.expired", DEFAULT_TIMEOUT)?;

    let rescuer = server
        .worker()
        .id("rescuer")
        .queue("imports")
        .recording("import")
        .start()?;
    server.tick(Duration::from_secs(1))?;
    let done = wait_for_state(&client, id, JobState::Succeeded, DEFAULT_TIMEOUT)?;
    assert_eq!(done.attempt, 2);
    assert_eq!(done.result, Some(json!({"file": "a.csv"})));
    let rescued = rescuer.recorder().of_job(id);
    assert_eq!(rescued.len(), 1);
    assert_eq!(rescued[0].attempt, 2);
    assert_eq!(
        events.types_of_job(id),
        [
            "job.enqueued",
            "job.started",
            "job.expired",
            "job.failed",
            "job.started",
            "job.succeeded"
        ]
    );
    gate.open();
    Ok(())
}
