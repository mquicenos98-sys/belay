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
