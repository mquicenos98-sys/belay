//! What a tick does with due schedules: on-time fires, misfires after
//! downtime, overlap policies, pauses, bounds, jitter, dispatcher failures,
//! manual triggers, and a crash between dispatching and saving.

mod support;

use std::sync::Arc;
use std::time::Duration;

use belay_json::Value;
use belay_proto::job::{META_FIRE_TIME, META_SCHEDULE};
use belay_proto::{JobState, MisfirePolicy, OverlapPolicy, UniqueScope};
use belay_scheduler::{
    jitter_offset, DispatchError, Dispatcher as _, Error, SchedulerOptions, SkipReason,
    TickErrorKind, META_TRIGGER,
};
use belay_store::MemKv;

use support::{fired_times, id, spec, ts, FlakyKv, Harness};

const MONDAY: &str = "2025-06-02T10:07:00Z";

#[test]
fn a_due_fire_dispatches_a_stamped_job() {
    let h = Harness::new(MONDAY);
    h.scheduler.create(spec("sync", "*/15 * * * *")).unwrap();
    h.events();
    assert!(h.tick_at("2025-06-02T10:14:59.999Z").is_empty());

    let report = h.tick_at("2025-06-02T10:15:00.300Z");
    assert_eq!(fired_times(&report), ["2025-06-02T10:15:00.000Z"]);
    let fired = &report.fired[0];
    assert!(!fired.duplicate && !fired.catch_up);
    assert_eq!(fired.run_at, None);

    let job = h.queue.get(&fired.job).unwrap();
    assert_eq!(job.spec.kind, "work");
    assert_eq!(job.spec.meta[META_SCHEDULE], "sync");
    assert_eq!(job.spec.meta[META_FIRE_TIME], "2025-06-02T10:15:00.000Z");
    let unique = job.spec.unique.unwrap();
    assert_eq!(unique.key, "sched:sync:1748859300000");
    assert_eq!(unique.scope, UniqueScope::For(Duration::from_secs(86_400)));

    let schedule = h.scheduler.get(&id("sync")).unwrap();
    assert_eq!(schedule.fire_count, 1);
    assert_eq!(schedule.last_job, Some(fired.job));
    assert_eq!(schedule.last_fire_at, Some(ts("2025-06-02T10:15:00Z")));
    assert_eq!(schedule.next_fire_at, Some(ts("2025-06-02T10:30:00Z")));
    assert_eq!(h.events(), ["schedule.fired"]);
    assert_eq!(report.to_string(), "1 fired, 0 skipped, 0 errors");

    // Ticking again at the same moment does nothing.
    assert!(h.scheduler.tick(h.now()).is_empty());
    assert_eq!(h.queue.job_count(), 1);
}

#[test]
fn ticking_at_every_wakeup_fires_every_occurrence() {
    let h = Harness::new(MONDAY);
    h.scheduler.create(spec("hourly", "0 * * * *")).unwrap();
    h.scheduler.create(spec("quarter", "*/15 * * * *")).unwrap();
    let reports = h.run_until("2025-06-03T10:07:00Z");
    let fired: Vec<_> = reports.iter().flat_map(|r| r.fired.iter()).collect();
    assert_eq!(fired.iter().filter(|f| f.schedule == "hourly").count(), 24);
    assert_eq!(fired.iter().filter(|f| f.schedule == "quarter").count(), 96);
    assert!(fired.iter().all(|f| !f.catch_up));
    // Where both fire in one tick, the order is by fire time, then id.
    let both = reports.iter().find(|r| r.fired.len() == 2).unwrap();
    assert_eq!(both.fired[0].schedule, "hourly");
    assert_eq!(h.queue.job_count(), 120);
}

#[test]
fn misfire_skip_drops_everything_missed() {
    let h = Harness::new(MONDAY);
    h.scheduler
        .create(spec("hourly", "0 * * * *").misfire(MisfirePolicy::Skip))
        .unwrap();
    h.events();
    // Down from Monday 10:07 until Tuesday 15:30: 11:00 Monday through
    // 15:00 Tuesday were missed, 29 fires.
    let report = h.tick_at("2025-06-03T15:30:00Z");
    assert!(report.fired.is_empty());
    let skipped = &report.skipped[0];
    assert_eq!(skipped.reason, SkipReason::Misfire);
    assert_eq!(skipped.fire_time, ts("2025-06-02T11:00:00Z"));
    assert_eq!(skipped.last_fire_time, ts("2025-06-03T15:00:00Z"));
    assert_eq!(skipped.count, 29);
    assert_eq!(h.events(), ["schedule.misfired"]);
    assert_eq!(
        h.scheduler.get(&id("hourly")).unwrap().next_fire_at,
        Some(ts("2025-06-03T16:00:00Z"))
    );
    assert_eq!(h.queue.job_count(), 0);
}

#[test]
fn misfire_fire_once_fires_the_first_missed_time_after_a_restart() {
    let h = Harness::new(MONDAY);
    h.scheduler.create(spec("hourly", "0 * * * *")).unwrap();
    assert_eq!(h.tick_at("2025-06-02T11:00:00Z").fired.len(), 1);
    // belay stops, and comes back hours later.
    let h = h.restart();
    h.events();
    let report = h.tick_at("2025-06-02T15:30:00Z");
    assert_eq!(fired_times(&report), ["2025-06-02T12:00:00.000Z"]);
    assert!(report.fired[0].catch_up);
    let skipped = &report.skipped[0];
    assert_eq!(
        (
            skipped.reason,
            skipped.fire_time,
            skipped.last_fire_time,
            skipped.count
        ),
        (
            SkipReason::Misfire,
            ts("2025-06-02T13:00:00Z"),
            ts("2025-06-02T15:00:00Z"),
            3
        )
    );
    let events = h.sink.take();
    match &events[0] {
        belay_proto::event::EventKind::ScheduleMisfired {
            fire_time,
            missed,
            policy,
            ..
        } => {
            assert_eq!(*fire_time, ts("2025-06-02T12:00:00Z"));
            assert_eq!(*missed, 4);
            assert_eq!(*policy, MisfirePolicy::FireOnce);
        }
        other => panic!("expected a misfire event, got {other:?}"),
    }
    assert_eq!(events[1].type_name(), "schedule.fired");
    assert_eq!(h.queue.job_count(), 2);
}

#[test]
fn misfire_fire_all_catches_up_oldest_first_up_to_max() {
    let h = Harness::new(MONDAY);
    h.scheduler
        .create(spec("hourly", "0 * * * *").misfire(MisfirePolicy::FireAll { max: 5 }))
        .unwrap();
    let report = h.tick_at("2025-06-03T15:30:00Z");
    assert_eq!(
        fired_times(&report),
        [
            "2025-06-02T11:00:00.000Z",
            "2025-06-02T12:00:00.000Z",
            "2025-06-02T13:00:00.000Z",
            "2025-06-02T14:00:00.000Z",
            "2025-06-02T15:00:00.000Z"
        ]
    );
    assert!(report.fired.iter().all(|f| f.catch_up));
    let skipped = &report.skipped[0];
    assert_eq!(skipped.fire_time, ts("2025-06-02T16:00:00Z"));
    assert_eq!(skipped.count, 24);
    // Each catch-up job is stamped with its own fire time.
    let stamps: Vec<String> = h
        .queue
        .jobs()
        .iter()
        .map(|j| j.spec.meta[META_FIRE_TIME].clone())
        .collect();
    assert_eq!(stamps[4], "2025-06-02T15:00:00.000Z");
}

#[test]
fn fire_all_with_room_fires_every_missed_time() {
    let h = Harness::new(MONDAY);
    h.scheduler
        .create(spec("hourly", "0 * * * *").misfire(MisfirePolicy::FireAll { max: 100 }))
        .unwrap();
    let report = h.tick_at("2025-06-02T13:30:00Z");
    assert_eq!(report.fired.len(), 3);
    assert!(report.skipped.is_empty());
}

#[test]
fn lateness_within_the_threshold_is_not_a_misfire() {
    let h = Harness::new(MONDAY);
    h.scheduler
        .create(spec("sync", "*/15 * * * *").misfire(MisfirePolicy::Skip))
        .unwrap();
    let late = h.tick_at("2025-06-02T10:16:00Z");
    assert_eq!(late.fired.len(), 1, "60s late is still on time");
    assert!(!late.fired[0].catch_up);
    let too_late = h.tick_at("2025-06-02T10:31:00.001Z");
    assert!(too_late.fired.is_empty());
    assert_eq!(too_late.skipped[0].reason, SkipReason::Misfire);

    let patient = Harness::with(
        MONDAY,
        Arc::new(MemKv::new()),
        SchedulerOptions::new().misfire_threshold(Duration::from_secs(600)),
    );
    patient
        .scheduler
        .create(spec("sync", "*/15 * * * *").misfire(MisfirePolicy::Skip))
        .unwrap();
    let report = patient.tick_at("2025-06-02T10:24:00Z");
    assert_eq!(
        report.fired.len(),
        1,
        "a 10 minute threshold forgives 9 minutes"
    );
}

// Regression: a long outage of a frequent schedule used to lump the fires
// still within the threshold into the misfire.
#[test]
fn a_long_backlog_saturates_the_count_but_recent_fires_stay_on_time() {
    let h = Harness::new(MONDAY);
    h.scheduler
        .create(spec("minutely", "* * * * *").misfire(MisfirePolicy::Skip))
        .unwrap();
    // Ten days: 14,400 minutes.
    let report = h.tick_at("2025-06-12T10:07:30Z");
    let skipped = &report.skipped[0];
    assert_eq!(skipped.count, 10_000);
    assert_eq!(skipped.fire_time, ts("2025-06-02T10:08:00Z"));
    assert_eq!(skipped.last_fire_time, ts("2025-06-12T10:06:00Z"));
    assert_eq!(fired_times(&report), ["2025-06-12T10:07:00.000Z"]);
    assert_eq!(
        h.scheduler.get(&id("minutely")).unwrap().next_fire_at,
        Some(ts("2025-06-12T10:08:00Z"))
    );
}

#[test]
fn overlap_skip_waits_for_the_previous_job() {
    let h = Harness::new(MONDAY);
    h.scheduler
        .create(spec("sync", "*/5 * * * *").overlap(OverlapPolicy::Skip))
        .unwrap();
    let first = h.tick_at("2025-06-02T10:10:00Z").fired[0].job;
    h.events();
    let blocked = h.tick_at("2025-06-02T10:15:00Z");
    assert!(blocked.fired.is_empty());
    assert_eq!(blocked.skipped[0].reason, SkipReason::Overlap);
    assert_eq!(blocked.skipped[0].fire_time, ts("2025-06-02T10:15:00Z"));
    assert_eq!(h.events(), ["schedule.skipped"]);
    // The skip still moves the schedule on.
    assert_eq!(
        h.scheduler.get(&id("sync")).unwrap().next_fire_at,
        Some(ts("2025-06-02T10:20:00Z"))
    );
    h.queue.succeed(&first, Value::Null);
    assert_eq!(h.tick_at("2025-06-02T10:20:00Z").fired.len(), 1);
}

#[test]
fn overlap_allow_fires_regardless() {
    let h = Harness::new(MONDAY);
    h.scheduler.create(spec("sync", "*/5 * * * *")).unwrap();
    assert_eq!(h.tick_at("2025-06-02T10:10:00Z").fired.len(), 1);
    assert_eq!(h.tick_at("2025-06-02T10:15:00Z").fired.len(), 1);
    assert_eq!(h.queue.job_count(), 2);
}

#[test]
fn overlap_cancel_previous_cancels_then_fires() {
    let h = Harness::new(MONDAY);
    h.scheduler
        .create(spec("sync", "*/5 * * * *").overlap(OverlapPolicy::CancelPrevious))
        .unwrap();
    let first = h.tick_at("2025-06-02T10:10:00Z").fired[0].job;
    h.queue.start(&first);
    let second = h.tick_at("2025-06-02T10:15:00Z").fired[0].job;
    // A running job is asked to stop through its worker.
    assert_eq!(h.queue.cancel_requests(), [first]);
    // A job that has not started is cancelled outright.
    let third = h.tick_at("2025-06-02T10:20:00Z");
    assert_eq!(third.fired.len(), 1);
    assert_eq!(h.queue.job_state(&second), Some(JobState::Cancelled));
    assert!(third.errors.is_empty());
}

#[test]
fn catch_up_fires_blocked_by_overlap_are_reported_together() {
    let h = Harness::new(MONDAY);
    h.scheduler
        .create(
            spec("hourly", "0 * * * *")
                .misfire(MisfirePolicy::FireAll { max: 10 })
                .overlap(OverlapPolicy::Skip),
        )
        .unwrap();
    h.events();
    let report = h.tick_at("2025-06-02T14:30:00Z");
    assert_eq!(fired_times(&report), ["2025-06-02T11:00:00.000Z"]);
    assert_eq!(report.skipped.len(), 1);
    let skipped = &report.skipped[0];
    assert_eq!(skipped.reason, SkipReason::Overlap);
    assert_eq!(
        (skipped.fire_time, skipped.last_fire_time, skipped.count),
        (ts("2025-06-02T12:00:00Z"), ts("2025-06-02T14:00:00Z"), 3)
    );
    assert_eq!(
        h.events(),
        [
            "schedule.misfired",
            "schedule.fired",
            "schedule.skipped",
            "schedule.skipped",
            "schedule.skipped"
        ]
    );
}

#[test]
fn paused_schedules_pass_their_fires_without_catching_up() {
    let h = Harness::new(MONDAY);
    h.scheduler.create(spec("sync", "*/15 * * * *")).unwrap();
    h.scheduler.pause(&id("sync")).unwrap();
    let reports = h.run_until("2025-06-02T11:00:00Z");
    let skipped: Vec<_> = reports.iter().flat_map(|r| r.skipped.iter()).collect();
    assert_eq!(skipped.len(), 4, "10:15, 10:30, 10:45 and 11:00");
    assert!(skipped.iter().all(|s| s.reason == SkipReason::Paused));
    assert_eq!(h.queue.job_count(), 0);

    let resumed = h.scheduler.resume(&id("sync")).unwrap();
    assert_eq!(resumed.next_fire_at, Some(ts("2025-06-02T11:15:00Z")));
    let report = h.tick_at("2025-06-02T11:15:00Z");
    assert_eq!(fired_times(&report), ["2025-06-02T11:15:00.000Z"]);
    assert!(report.skipped.is_empty());
}

#[test]
fn resuming_after_downtime_does_not_catch_up() {
    let h = Harness::new(MONDAY);
    h.scheduler.create(spec("sync", "*/15 * * * *")).unwrap();
    h.scheduler.pause(&id("sync")).unwrap();
    let h = h.restart();
    h.clock.set(ts("2025-06-04T08:05:00Z"));
    let resumed = h.scheduler.resume(&id("sync")).unwrap();
    assert_eq!(resumed.next_fire_at, Some(ts("2025-06-04T08:15:00Z")));
    assert!(h.scheduler.tick(h.now()).is_empty());
}

#[test]
fn fires_stop_at_ends_at() {
    let h = Harness::new(MONDAY);
    h.scheduler
        .create(spec("brief", "0 * * * *").between(None, Some(ts("2025-06-02T13:00:00Z"))))
        .unwrap();
    let reports = h.run_until("2025-06-03T00:00:00Z");
    let times: Vec<String> = reports.iter().flat_map(fired_times).collect();
    assert_eq!(
        times,
        ["2025-06-02T11:00:00.000Z", "2025-06-02T12:00:00.000Z"]
    );
    assert_eq!(h.scheduler.get(&id("brief")).unwrap().next_fire_at, None);
    assert_eq!(h.scheduler.next_wakeup(), None);
}

#[test]
fn jitter_delays_each_job_by_a_stable_offset_within_the_bound() {
    let jitter = Duration::from_secs(600);
    let run = || {
        let h = Harness::new(MONDAY);
        h.scheduler
            .create(spec("report", "0 * * * *").jitter(jitter))
            .unwrap();
        let reports = h.run_until("2025-06-03T10:07:00Z");
        let fired: Vec<_> = reports.into_iter().flat_map(|r| r.fired).collect();
        (h, fired)
    };
    let (h, fired) = run();
    assert_eq!(fired.len(), 24);
    let mut offsets = Vec::new();
    for f in &fired {
        let offset = jitter_offset(&id("report"), f.fire_time, jitter);
        assert!(offset < jitter);
        assert_eq!(f.run_at, Some(f.fire_time + offset));
        let job = h.queue.get(&f.job).unwrap();
        assert_eq!(job.spec.run_at, f.run_at);
        assert_eq!(job.state, JobState::Scheduled, "held until its run_at");
        offsets.push(offset);
    }
    offsets.sort();
    offsets.dedup();
    assert!(offsets.len() > 20, "offsets vary between fires");
    // A second, independent scheduler computes the very same delays.
    let (_, again) = run();
    let a: Vec<_> = fired.iter().map(|f| f.run_at).collect();
    let b: Vec<_> = again.iter().map(|f| f.run_at).collect();
    assert_eq!(a, b);
}

#[test]
fn a_template_delay_is_added_to_the_jitter() {
    let h = Harness::new(MONDAY);
    let mut s = spec("slow", "0 * * * *").jitter(Duration::from_secs(60));
    s.template = s.template.delay(Duration::from_secs(3600));
    h.scheduler.create(s).unwrap();
    let fired = &h.tick_at("2025-06-02T11:00:00Z").fired[0];
    let offset = jitter_offset(&id("slow"), fired.fire_time, Duration::from_secs(60));
    let job = h.queue.get(&fired.job).unwrap();
    assert_eq!(job.spec.run_at, Some(ts("2025-06-02T12:00:00Z") + offset));
    assert_eq!(job.spec.delay, None);
}

#[test]
fn rejected_fires_are_dropped_and_reported() {
    let h = Harness::new(MONDAY);
    h.scheduler.create(spec("sync", "*/15 * * * *")).unwrap();
    h.queue.fail_next(DispatchError::Rejected(
        "queue jobs is being deleted".into(),
    ));
    let report = h.tick_at("2025-06-02T10:15:00Z");
    assert!(report.fired.is_empty());
    let error = &report.errors[0];
    assert_eq!(error.kind, TickErrorKind::DispatchRejected);
    assert_eq!(
        error.to_string(),
        "dispatch rejected for schedule sync at 2025-06-02T10:15:00.000Z: queue jobs is being deleted"
    );
    let schedule = h.scheduler.get(&id("sync")).unwrap();
    assert_eq!(schedule.fire_count, 0);
    assert_eq!(schedule.next_fire_at, Some(ts("2025-06-02T10:30:00Z")));
    assert_eq!(h.tick_at("2025-06-02T10:30:00Z").fired.len(), 1);
}

#[test]
fn an_unavailable_dispatcher_is_retried_after_the_delay() {
    let h = Harness::new(MONDAY);
    h.scheduler.create(spec("sync", "*/15 * * * *")).unwrap();
    h.queue
        .fail_next(DispatchError::Unavailable("queue is restarting".into()));
    let report = h.tick_at("2025-06-02T10:15:00Z");
    assert_eq!(report.errors[0].kind, TickErrorKind::DispatchUnavailable);
    assert_eq!(
        h.scheduler.get(&id("sync")).unwrap().next_fire_at,
        Some(ts("2025-06-02T10:15:00Z")),
        "the fire is kept"
    );
    assert_eq!(h.scheduler.next_wakeup(), Some(ts("2025-06-02T10:15:05Z")));
    assert!(h.tick_at("2025-06-02T10:15:03Z").is_empty(), "backing off");
    let retried = h.tick_at("2025-06-02T10:15:05Z");
    assert_eq!(fired_times(&retried), ["2025-06-02T10:15:00.000Z"]);
    assert!(!retried.fired[0].catch_up);
    assert_eq!(h.queue.dispatch_calls(), 2);
}

#[test]
fn trigger_now_fires_at_once_and_is_marked_manual() {
    let h = Harness::new(MONDAY);
    h.scheduler.create(spec("sync", "*/15 * * * *")).unwrap();
    h.scheduler.pause(&id("sync")).unwrap();
    h.events();
    let fired = h.scheduler.trigger_now(&id("sync")).unwrap();
    assert_eq!(fired.fire_time, ts(MONDAY));
    let job = h.queue.get(&fired.job).unwrap();
    assert_eq!(job.spec.meta[META_TRIGGER], "manual");
    assert_eq!(job.spec.meta[META_FIRE_TIME], "2025-06-02T10:07:00.000Z");
    assert_eq!(
        job.spec.unique.unwrap().key,
        "sched:sync:1748858820000:manual"
    );
    let schedule = h.scheduler.get(&id("sync")).unwrap();
    assert_eq!(schedule.fire_count, 1);
    assert_eq!(schedule.last_job, Some(fired.job));
    assert_eq!(schedule.next_fire_at, Some(ts("2025-06-02T10:15:00Z")));
    assert_eq!(h.events(), ["schedule.fired"]);

    assert!(h
        .scheduler
        .trigger_now(&id("ghost"))
        .unwrap_err()
        .is_not_found());
    h.queue.fail_next(DispatchError::Unavailable("busy".into()));
    let err = h.scheduler.trigger_now(&id("sync")).unwrap_err();
    assert!(matches!(
        err,
        Error::Dispatch(DispatchError::Unavailable(_))
    ));
    assert_eq!(h.scheduler.get(&id("sync")).unwrap().fire_count, 1);
}

#[test]
fn a_crash_between_dispatch_and_save_does_not_fire_twice() {
    let flaky = FlakyKv::new(Arc::new(MemKv::new()));
    let h = Harness::with(MONDAY, flaky.clone(), SchedulerOptions::new());
    h.scheduler
        .create(spec("sync", "*/15 * * * *").jitter(Duration::from_secs(90)))
        .unwrap();
    flaky.refuse_writes(true);
    let first = h.tick_at("2025-06-02T10:15:00Z");
    assert_eq!(first.fired.len(), 1);
    assert_eq!(first.errors[0].kind, TickErrorKind::Store);
    let job = first.fired[0].job;

    // The process dies here: the job exists, the bookkeeping does not.
    flaky.refuse_writes(false);
    let h = h.restart();
    let reloaded = h.scheduler.get(&id("sync")).unwrap();
    assert_eq!((reloaded.fire_count, reloaded.last_job), (0, None));

    let retry = h.tick_at("2025-06-02T10:15:02Z");
    assert_eq!(retry.fired.len(), 1);
    assert!(retry.fired[0].duplicate);
    assert_eq!(retry.fired[0].job, job);
    assert_eq!(retry.fired[0].run_at, first.fired[0].run_at);
    assert_eq!(h.queue.job_count(), 1);
    let saved = h.scheduler.get(&id("sync")).unwrap();
    assert_eq!(saved.fire_count, 1);
    assert_eq!(saved.last_job, Some(job));
    assert_eq!(saved.next_fire_at, Some(ts("2025-06-02T10:30:00Z")));
}

#[test]
fn a_crash_then_downtime_still_yields_one_job_per_fire() {
    for (policy, jobs) in [
        (MisfirePolicy::FireOnce, 1),
        (MisfirePolicy::FireAll { max: 10 }, 4),
    ] {
        let flaky = FlakyKv::new(Arc::new(MemKv::new()));
        let h = Harness::with(MONDAY, flaky.clone(), SchedulerOptions::new());
        h.scheduler
            .create(spec("hourly", "0 * * * *").misfire(policy))
            .unwrap();
        flaky.refuse_writes(true);
        let job = h.tick_at("2025-06-02T11:00:00Z").fired[0].job;
        flaky.refuse_writes(false);
        // Down until 14:30: 11:00 (already dispatched), 12, 13 and 14 are
        // all misfires now.
        let h = h.restart();
        let report = h.tick_at("2025-06-02T14:30:00Z");
        assert_eq!(report.fired[0].job, job, "{policy}");
        assert!(report.fired[0].duplicate, "{policy}");
        assert_eq!(h.queue.job_count(), jobs, "{policy}");
    }
}

#[test]
fn a_failed_save_is_retried_by_the_same_scheduler() {
    let flaky = FlakyKv::new(Arc::new(MemKv::new()));
    let h = Harness::with(MONDAY, flaky.clone(), SchedulerOptions::new());
    h.scheduler.create(spec("sync", "*/15 * * * *")).unwrap();
    h.events();
    flaky.refuse_writes(true);
    let failed = h.tick_at("2025-06-02T10:15:00Z");
    assert_eq!(failed.errors.len(), 1);
    assert!(
        h.events().is_empty(),
        "nothing is announced before it is saved"
    );
    assert_eq!(h.scheduler.next_wakeup(), Some(ts("2025-06-02T10:15:05Z")));
    flaky.refuse_writes(false);
    let retried = h.tick_at("2025-06-02T10:15:05Z");
    assert!(retried.fired[0].duplicate);
    assert_eq!(h.events(), ["schedule.fired"]);
    assert_eq!(h.scheduler.get(&id("sync")).unwrap().fire_count, 1);
    assert_eq!(h.queue.job_count(), 1);
    assert_eq!(
        h.queue.job_state(&retried.fired[0].job),
        Some(JobState::Ready)
    );
}

#[test]
fn concurrent_ticks_never_fire_the_same_time_twice() {
    let h = Harness::new(MONDAY);
    for i in 0..8 {
        h.scheduler
            .create(spec(&format!("s{i}"), "*/15 * * * *"))
            .unwrap();
    }
    h.clock.set(ts("2025-06-02T10:15:00Z"));
    let now = h.now();
    let fired: usize = std::thread::scope(|scope| {
        let ticks: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| h.scheduler.tick(now).fired.len()))
            .collect();
        ticks.into_iter().map(|t| t.join().unwrap()).sum()
    });
    assert_eq!(fired, 8);
    assert_eq!(h.queue.job_count(), 8);
    assert_eq!(h.queue.dispatch_calls(), 8);
}
