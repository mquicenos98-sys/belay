//! A short, bounded soak in real time: 2,000 jobs across four queues,
//! three workers with four slots each, and handlers that fail at random.
//! Every job must end succeeded or dead exactly once (counted on the event
//! stream), the one predicted by the handlers' deterministic failures, and
//! no job may ever be leased twice at once.

use std::collections::{BTreeMap, BTreeSet};
use std::thread;
use std::time::Duration;

use belay_common::BackoffPolicy;
use belay_testkit::prelude::*;

const QUEUES: [&str; 4] = ["soak-a", "soak-b", "soak-c", "soak-d"];
const PER_QUEUE: usize = 500;
const BATCH: usize = 100;
const MAX_ATTEMPTS: u32 = 3;
const FAILURE_RATE: f64 = 0.3;
const SEED: u64 = 2025;

/// Whether the handlers fail every attempt of `id`, which sends it to
/// `dead`.
fn predicted_dead(id: JobId) -> bool {
    (1..=MAX_ATTEMPTS).all(|attempt| Behavior::flaky_fails(id, attempt, FAILURE_RATE, SEED))
}

fn enqueue_all(url: &str, queue: &'static str) -> Result<Vec<JobId>> {
    let client = belay_testkit::connect(url, None)?;
    let name = QueueName::from_static(queue);
    let mut ids = Vec::with_capacity(PER_QUEUE);
    for chunk in 0..PER_QUEUE / BATCH {
        let specs: Vec<JobSpec> = (0..BATCH)
            .map(|n| {
                JobSpec::new(name.clone(), "work")
                    .payload(json!({"queue": queue, "n": chunk * BATCH + n}))
                    .max_attempts(MAX_ATTEMPTS)
                    .retry(BackoffPolicy::constant(Duration::from_millis(1)))
            })
            .collect();
        ids.extend(
            client
                .jobs()
                .enqueue_batch(&specs)?
                .into_iter()
                .map(|r| r.id),
        );
    }
    Ok(ids)
}

#[test]
fn two_thousand_jobs_with_random_failures_each_end_exactly_once() -> Result<()> {
    let total = QUEUES.len() * PER_QUEUE;
    let server = TestServer::builder()
        .system_clock()
        .configure(|config| {
            config.events.buffer = 100_000;
            config.events.subscriber_queue = 100_000;
        })
        .start()?;
    let events = server.event_log()?;
    let recorder = Recorder::new();
    let workers: Vec<TestWorker> = (0..3)
        .map(|i| {
            server
                .worker()
                .id(&format!("soak-{i}"))
                .queues(QUEUES)
                .concurrency(4)
                .recorder(&recorder)
                .flaky("work", FAILURE_RATE, SEED)
                .start()
        })
        .collect::<Result<_>>()?;

    let producers: Vec<_> = QUEUES
        .iter()
        .map(|queue| {
            let url = server.url();
            thread::spawn(move || enqueue_all(&url, queue))
        })
        .collect();
    let mut ids: Vec<JobId> = Vec::with_capacity(total);
    for producer in producers {
        ids.extend(producer.join().expect("a producer panicked")?);
    }
    assert_eq!(ids.len(), total);

    eventually(Duration::from_secs(90), || {
        events.count("job.succeeded") + events.count("job.dead") >= total
    })?;

    // Exactly one final event per job, and the one predicted.
    let mut finals: BTreeMap<JobId, Vec<&'static str>> = BTreeMap::new();
    let mut timeline: BTreeMap<JobId, Vec<&'static str>> = BTreeMap::new();
    for event in events.events() {
        let Some(job) = event.kind.job() else {
            continue;
        };
        let name = event.type_name();
        if matches!(name, "job.succeeded" | "job.dead") {
            finals.entry(job).or_default().push(name);
        }
        timeline.entry(job).or_default().push(name);
    }
    let expected_ids: BTreeSet<JobId> = ids.iter().copied().collect();
    assert_eq!(
        finals.keys().copied().collect::<BTreeSet<_>>(),
        expected_ids
    );
    let mut dead = 0;
    for (job, ends) in &finals {
        assert_eq!(ends.len(), 1, "{job} ended {ends:?}");
        let expected = if predicted_dead(*job) {
            dead += 1;
            "job.dead"
        } else {
            "job.succeeded"
        };
        assert_eq!(ends[0], expected, "{job}");
    }
    // With a 30% failure rate and three attempts, about 2.7% die.
    assert!((10..150).contains(&dead), "{dead} dead jobs");

    // No job was ever leased twice at once: every start is followed by an
    // end of that attempt before the next start, and no handler ran a job
    // while another handler had it.
    for (job, events) in &timeline {
        let mut open = false;
        for name in events {
            match *name {
                "job.started" => {
                    assert!(!open, "{job} started twice: {events:?}");
                    open = true;
                }
                "job.failed" | "job.succeeded" | "job.dead" | "job.expired" => open = false,
                _ => {}
            }
        }
    }
    assert!(recorder.overlaps().is_empty(), "{:?}", recorder.overlaps());
    assert_eq!(events.count("job.expired"), 0);

    // Attempts agree between the stream, the handlers and the server:
    // every attempt but a job's last ends in `job.failed`, and the last in
    // `job.succeeded` or `job.dead`.
    let starts = events.count("job.started");
    assert_eq!(recorder.len(), starts);
    assert_eq!(events.count("job.failed") + total, starts);
    let client = server.client();
    for queue in QUEUES {
        let counts = eventually_some(DEFAULT_TIMEOUT, "the queue to settle", || {
            client
                .queues()
                .get(&QueueName::from_static(queue))
                .ok()
                .map(|info| info.counts)
                .filter(|c| c.get(JobState::Succeeded) + c.get(JobState::Dead) == PER_QUEUE as u64)
        })?;
        assert_eq!(counts.total(), PER_QUEUE as u64, "{queue}: {counts:?}");
    }
    let finished: u64 = workers.iter().map(|w| w.stats().succeeded).sum();
    assert_eq!(finished as usize, total - dead);
    Ok(())
}
