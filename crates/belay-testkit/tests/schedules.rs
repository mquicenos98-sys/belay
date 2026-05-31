//! Cron schedules over the HTTP API, walked with a manual clock: a named
//! zone across both daylight-saving transitions, each misfire policy after
//! the server was down for hours, and pausing and triggering by hand.

use std::time::Duration;

use belay_common::Timestamp;
use belay_proto::job::{META_FIRE_TIME, META_SCHEDULE};
use belay_proto::Retention;
use belay_testkit::fixtures::SCHEDULED_QUEUE;
use belay_testkit::prelude::*;

fn at(text: &str) -> Timestamp {
    Timestamp::parse_rfc3339(text).expect("test timestamps are valid RFC 3339")
}

fn fire_time(job: &Job) -> Option<Timestamp> {
    job.spec
        .meta
        .get(META_FIRE_TIME)
        .and_then(|t| Timestamp::parse_rfc3339(t).ok())
}

/// Marks jobs started by `POST /v1/schedules/:id/trigger`.
const META_TRIGGER: &str = "belay.trigger";

/// The jobs schedule `id` fired on its own (not triggered by hand), oldest
/// fire first.
fn fired_jobs(client: &Client, id: &str) -> Result<Vec<Job>> {
    let filter = JobFilter::new()
        .queue(QueueName::from_static(SCHEDULED_QUEUE))
        .kind(id);
    let mut jobs: Vec<Job> = client
        .jobs()
        .list_all(filter)
        .collect::<std::result::Result<_, _>>()?;
    jobs.retain(|j| !j.spec.meta.contains_key(META_TRIGGER));
    jobs.sort_by_key(fire_time);
    Ok(jobs)
}

fn fire_times(client: &Client, id: &str) -> Result<Vec<Timestamp>> {
    Ok(fired_jobs(client, id)?
        .iter()
        .filter_map(fire_time)
        .collect())
}

/// Keeps succeeded jobs of the schedules' queue for a month instead of the
/// default day, so a test walking over several days can still list them.
fn keep_fired_jobs(config: &mut belay_core::Config) {
    let retention = Retention {
        succeeded: Duration::from_secs(30 * 86_400),
        ..Retention::default()
    };
    config.queues.insert(
        QueueName::from_static(SCHEDULED_QUEUE),
        QueueConfig {
            retention,
            ..QueueConfig::default()
        },
    );
}

#[test]
fn a_new_york_schedule_fires_once_after_the_spring_forward_gap() -> Result<()> {
    let server = TestServer::builder()
        .starting_at(at("2025-03-08T12:00:00Z"))
        .configure(keep_fired_jobs)
        .start()?;
    let worker = server
        .worker()
        .queue(SCHEDULED_QUEUE)
        .recording("ny-report")
        .start()?;
    let client = server.client();
    let spec = schedule("ny-report", "30 2 * * *").timezone("America/New_York");
    let created = client.schedules().create(&spec)?;

    // 02:30 does not exist on 9 March: the fire moves to 03:00 EDT, the
    // first instant after the gap.
    let expected = [
        at("2025-03-09T07:00:00Z"),
        at("2025-03-10T06:30:00Z"),
        at("2025-03-11T06:30:00Z"),
    ];
    assert_eq!(created.next_fire_at, Some(expected[0]));
    assert_eq!(client.schedules().preview(&created.spec.id, 3)?, expected);

    // 01:30 EST, the last half hour before the gap: nothing yet.
    server.tick_at(at("2025-03-09T06:30:00Z"))?;
    assert!(fire_times(&client, "ny-report")?.is_empty());

    // Each fire's job finishes before the clock jumps a day: a jump past a
    // running job's lease would expire it.
    for (n, fire) in expected.iter().enumerate() {
        server.tick_at(*fire)?;
        let jobs = fired_jobs(&client, "ny-report")?;
        assert_eq!(jobs.len(), n + 1);
        let newest = &jobs[n];
        assert_eq!(fire_time(newest), Some(*fire));
        assert_eq!(
            newest.spec.meta.get(META_SCHEDULE).map(String::as_str),
            Some("ny-report")
        );
        let done = wait_for_state(&client, newest.id, JobState::Succeeded, DEFAULT_TIMEOUT)?;
        assert_eq!(done.started_at, Some(*fire));
    }
    assert_eq!(
        worker
            .recorder()
            .attempts_of(fired_jobs(&client, "ny-report")?[0].id),
        [1]
    );
    assert_eq!(worker.recorder().len(), 3);

    let after = client.schedules().get(&created.spec.id)?;
    assert_eq!(after.fire_count, 3);
    assert_eq!(after.last_fire_at, Some(expected[2]));
    assert_eq!(after.next_fire_at, Some(at("2025-03-12T06:30:00Z")));
    Ok(())
}

#[test]
fn a_new_york_schedule_fires_once_in_the_repeated_fall_back_hour() -> Result<()> {
    let server = TestServer::builder()
        .starting_at(at("2025-11-01T12:00:00Z"))
        .start()?;
    let events = server.event_log()?;
    let client = server.client();
    let daily = client
        .schedules()
        .create(&schedule("ny-nightly", "30 1 * * *").timezone("America/New_York"))?;
    // Sub-hour steps fire in both passes through 01:00-02:00, like cronie.
    let half_hourly = client
        .schedules()
        .create(&schedule("ny-half-hourly", "*/30 1 * * *").timezone("America/New_York"))?;

    assert_eq!(
        client.schedules().preview(&daily.spec.id, 2)?,
        [at("2025-11-02T05:30:00Z"), at("2025-11-03T06:30:00Z")]
    );
    assert_eq!(
        client.schedules().preview(&half_hourly.spec.id, 5)?,
        [
            at("2025-11-02T05:00:00Z"),
            at("2025-11-02T05:30:00Z"),
            at("2025-11-02T06:00:00Z"),
            at("2025-11-02T06:30:00Z"),
            at("2025-11-03T06:00:00Z"),
        ]
    );

    // 01:30 EDT fires; 01:30 EST an hour later does not.
    for t in [
        "2025-11-02T05:00:00Z",
        "2025-11-02T05:30:00Z",
        "2025-11-02T06:00:00Z",
        "2025-11-02T06:30:00Z",
    ] {
        server.tick_at(at(t))?;
    }
    assert_eq!(
        fire_times(&client, "ny-nightly")?,
        [at("2025-11-02T05:30:00Z")]
    );
    assert_eq!(fire_times(&client, "ny-half-hourly")?.len(), 4);

    server.tick_at(at("2025-11-03T06:30:00Z"))?;
    assert_eq!(
        fire_times(&client, "ny-nightly")?,
        [at("2025-11-02T05:30:00Z"), at("2025-11-03T06:30:00Z")]
    );
    events.wait_for(DEFAULT_TIMEOUT, |events| {
        events
            .iter()
            .filter(|e| e.type_name() == "schedule.fired")
            .count()
            >= 7
    })?;
    // The half-hourly schedule missed 06:00 on the 3rd by 30 minutes and
    // caught it up once (fire_once); nothing else was late.
    let misfired: Vec<String> = events
        .matching(|e| e.type_name() == "schedule.misfired")
        .iter()
        .filter_map(|e| e.kind.schedule().map(|s| s.to_string()))
        .collect();
    assert_eq!(misfired, ["ny-half-hourly"]);
    Ok(())
}

#[test]
fn a_paused_schedule_skips_its_fires_and_a_trigger_runs_it_at_once() -> Result<()> {
    let server = TestServer::start()?;
    let events = server.event_log()?;
    let worker = server
        .worker()
        .queue(SCHEDULED_QUEUE)
        .recording("heartbeat")
        .start()?;
    let client = server.client();
    let id = client
        .schedules()
        .create(&schedule("heartbeat", "*/5 * * * *"))?
        .spec
        .id;
    client.schedules().pause(&id)?;
    events.wait_for_event(DEFAULT_TIMEOUT, |e| e.type_name() == "schedule.paused")?;
    // A paused schedule moves through its fire times without firing, so
    // resuming later does not unleash a burst.
    let report = server.tick(Duration::from_secs(5 * 60))?;
    assert_eq!(report.schedules.skipped.len(), 1);
    assert!(fire_times(&client, "heartbeat")?.is_empty());
    assert_eq!(
        client.schedules().get(&id)?.next_fire_at,
        Some(server.now() + Duration::from_secs(5 * 60))
    );

    let triggered = client.schedules().trigger(&id)?;
    let job = wait_for_state(&client, triggered, JobState::Succeeded, DEFAULT_TIMEOUT)?;
    assert_eq!(
        job.spec.meta.get(META_TRIGGER).map(String::as_str),
        Some("manual")
    );

    client.schedules().resume(&id)?;
    server.tick(Duration::from_secs(5 * 60))?;
    let fired = fire_times(&client, "heartbeat")?;
    assert_eq!(fired, [server.now()]);
    worker
        .recorder()
        .wait_until(DEFAULT_TIMEOUT, |r| r.succeeded() == 2)?;
    let current = client.schedules().get(&id)?;
    assert!(!current.spec.paused);
    assert_eq!(current.last_fire_at, Some(server.now()));
    Ok(())
}

#[test]
fn a_calendar_keeps_a_schedule_to_office_hours_and_off_holidays() -> Result<()> {
    let server = TestServer::builder().configure(keep_fired_jobs).start()?;
    let client = server.client();
    // Weekdays 09:00-17:00 in Bogota (UTC-5, no daylight saving), and
    // Wednesday 5 March off.
    let office: belay_proto::CalendarSpec = belay_json::from_str(
        r#"{
            "name": "office",
            "timezone": "America/Bogota",
            "exclude_dates": ["2025-03-05"],
            "windows": [{"days": "mon-fri", "start": "09:00", "end": "17:00"}]
        }"#,
    )?;
    client.calendars().put(&office)?;
    // Every four hours UTC: 11:00 and 15:00 in Bogota fall in the window.
    let spec = schedule("office-sync", "0 */4 * * *").calendar("office");
    let id = client.schedules().create(&spec)?.spec.id;

    let expected = [
        at("2025-03-03T16:00:00Z"),
        at("2025-03-03T20:00:00Z"),
        at("2025-03-04T16:00:00Z"),
        at("2025-03-04T20:00:00Z"),
        at("2025-03-06T16:00:00Z"),
        at("2025-03-06T20:00:00Z"),
        at("2025-03-07T16:00:00Z"),
        at("2025-03-07T20:00:00Z"),
        at("2025-03-10T16:00:00Z"),
    ];
    assert_eq!(
        client.schedules().preview(&id, expected.len() as u32)?,
        expected
    );

    // Walking the clock over every four-hour mark fires only those.
    let mut t = at("2025-03-03T12:00:00Z");
    while t <= at("2025-03-10T16:00:00Z") {
        server.tick_at(t)?;
        t = t + Duration::from_secs(4 * 3600);
    }
    assert_eq!(fire_times(&client, "office-sync")?, expected);
    // A calendar in use cannot be deleted.
    let refused = client
        .calendars()
        .delete("office")
        .expect_err("still in use");
    assert_eq!(refused.code(), Some(ErrorCode::FailedPrecondition));
    Ok(())
}

#[test]
fn overlap_policies_decide_what_a_fire_does_while_the_last_job_runs() -> Result<()> {
    let server = TestServer::builder().configure(keep_fired_jobs).start()?;
    let events = server.event_log()?;
    let gate = Gate::new();
    // Leases outlast the minute jumps below, so running jobs stay running.
    let worker = server
        .worker()
        .queue(SCHEDULED_QUEUE)
        .gated("overlap-skip", &gate)
        .gated("overlap-cancel", &gate)
        .lease_for(Duration::from_secs(600))
        .start()?;
    let client = server.client();
    client
        .schedules()
        .create(&schedule("overlap-skip", "* * * * *").overlap(belay_proto::OverlapPolicy::Skip))?;
    client.schedules().create(
        &schedule("overlap-cancel", "* * * * *")
            .overlap(belay_proto::OverlapPolicy::CancelPrevious),
    )?;

    server.tick(Duration::from_secs(60))?;
    let first_skip = fired_jobs(&client, "overlap-skip")?[0].id;
    let first_cancel = fired_jobs(&client, "overlap-cancel")?[0].id;
    worker
        .recorder()
        .wait_for_start(first_skip, DEFAULT_TIMEOUT)?;
    worker
        .recorder()
        .wait_for_start(first_cancel, DEFAULT_TIMEOUT)?;

    // A minute later both jobs still run.
    let report = server.tick(Duration::from_secs(60))?;
    assert_eq!(report.schedules.skipped.len(), 1);
    assert_eq!(
        report.schedules.skipped[0].schedule.as_str(),
        "overlap-skip"
    );
    assert_eq!(fired_jobs(&client, "overlap-skip")?.len(), 1);
    let skipped =
        events.wait_for_event(DEFAULT_TIMEOUT, |e| e.type_name() == "schedule.skipped")?;
    assert_eq!(
        skipped.kind.schedule().map(|s| s.as_str()),
        Some("overlap-skip")
    );

    // cancel_previous asked the old job to stop, which reached its handler
    // through the worker's heartbeat, and fired a new one.
    let cancelled = wait_for_state(&client, first_cancel, JobState::Cancelled, DEFAULT_TIMEOUT)?;
    assert_eq!(cancelled.attempt, 1);
    assert!(cancelled.finished_at.is_some());
    let jobs = fired_jobs(&client, "overlap-cancel")?;
    assert_eq!(jobs.len(), 2);
    worker
        .recorder()
        .wait_for_start(jobs[1].id, DEFAULT_TIMEOUT)?;

    gate.open();
    wait_for_state(&client, first_skip, JobState::Succeeded, DEFAULT_TIMEOUT)?;
    wait_for_state(&client, jobs[1].id, JobState::Succeeded, DEFAULT_TIMEOUT)?;
    let outcomes: Vec<Recorded> = worker
        .recorder()
        .of_job(first_cancel)
        .into_iter()
        .map(|e| e.outcome)
        .collect();
    assert_eq!(outcomes, [Recorded::Cancelled]);
    Ok(())
}

#[test]
fn jitter_holds_a_fired_job_back_by_a_fixed_offset_within_its_bound() -> Result<()> {
    let server = TestServer::start()?;
    let worker = server
        .worker()
        .queue(SCHEDULED_QUEUE)
        .recording("jittery")
        .start()?;
    let client = server.client();
    let bound = Duration::from_secs(600);
    client
        .schedules()
        .create(&schedule("jittery", "0 * * * *").jitter(bound))?;

    let fire = at("2025-03-03T09:00:00Z");
    server.tick_at(fire)?;
    let job = fired_jobs(&client, "jittery")?
        .pop()
        .expect("the schedule fired at 09:00");
    assert_eq!(fire_time(&job), Some(fire));
    assert!(
        job.run_at > fire && job.run_at < fire + bound,
        "{}",
        job.run_at
    );
    assert_eq!(job.state, JobState::Scheduled);

    // The job waits for its own time, not the fire time.
    worker.wait_for_empty_leases(2, DEFAULT_TIMEOUT)?;
    assert!(worker.recorder().is_empty());
    server.tick_at(job.run_at)?;
    let done = wait_for_state(&client, job.id, JobState::Succeeded, DEFAULT_TIMEOUT)?;
    assert_eq!(done.started_at, Some(job.run_at));
    Ok(())
}
