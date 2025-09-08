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
