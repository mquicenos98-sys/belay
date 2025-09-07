//! Schedules in zones with daylight saving time, and calendars: holidays,
//! business-hours windows, calendars in a zone other than the schedule's,
//! and how calendar vetoes are reported.

mod support;

use std::sync::Arc;

use belay_json::ToJson;
use belay_proto::event::{EventKind, SkipReason as EventSkipReason};
use belay_proto::job::META_FIRE_TIME;
use belay_proto::{CalendarSpec, MisfirePolicy, Schedule};
use belay_scheduler::{SkipReason, Skipped, TickReport};
use belay_store::Kv;

use support::{id, spec, ts, Harness};

fn fired(reports: &[TickReport]) -> Vec<String> {
    reports
        .iter()
        .flat_map(|r| r.fired.iter().map(|f| f.fire_time.to_string()))
        .collect()
}

fn skipped(reports: &[TickReport]) -> Vec<Skipped> {
    reports.iter().flat_map(|r| r.skipped.clone()).collect()
}

#[test]
fn daily_0230_in_new_york_across_spring_forward() {
    let h = Harness::new("2025-03-07T12:00:00Z");
    h.scheduler
        .create(spec("nightly", "30 2 * * *").timezone("America/New_York"))
        .unwrap();
    let preview: Vec<String> = h
        .scheduler
        .preview(&id("nightly"), 4)
        .unwrap()
        .iter()
        .map(ToString::to_string)
        .collect();
    let reports = h.run_until("2025-03-11T12:00:00Z");
    let times = fired(&reports);
    assert_eq!(
        times,
        [
            // 02:30 EST.
            "2025-03-08T07:30:00.000Z",
            // 02:30 does not exist on the 9th; the job runs at 03:00 EDT,
            // the first instant after the gap.
            "2025-03-09T07:00:00.000Z",
            // 02:30 EDT from then on.
            "2025-03-10T06:30:00.000Z",
            "2025-03-11T06:30:00.000Z",
        ]
    );
    assert_eq!(preview, times, "the preview agreed beforehand");
    let gap_job = h
        .queue
        .jobs()
        .into_iter()
        .find(|j| j.spec.meta[META_FIRE_TIME] == "2025-03-09T07:00:00.000Z");
    assert!(gap_job.is_some());
}

#[test]
fn daily_0230_in_new_york_across_fall_back() {
    let h = Harness::new("2025-10-31T12:00:00Z");
    h.scheduler
        .create(spec("nightly", "30 2 * * *").timezone("America/New_York"))
        .unwrap();
    let reports = h.run_until("2025-11-03T12:00:00Z");
    // 02:30 lies after the repeated hour (01:00-02:00), so it happens once
    // a day; the day of the change is 25 hours long.
    assert_eq!(
        fired(&reports),
        [
            "2025-11-01T06:30:00.000Z",
            "2025-11-02T07:30:00.000Z",
            "2025-11-03T07:30:00.000Z",
        ]
    );
}

#[test]
fn a_time_inside_the_repeated_hour_fires_once() {
    let h = Harness::new("2025-10-31T12:00:00Z");
    h.scheduler
        .create(spec("early", "30 1 * * *").timezone("America/New_York"))
        .unwrap();
    let reports = h.run_until("2025-11-03T12:00:00Z");
    // 01:30 happens twice on November 2nd (EDT, then EST); only the first
    // pass fires.
    assert_eq!(
        fired(&reports),
        [
            "2025-11-01T05:30:00.000Z",
            "2025-11-02T05:30:00.000Z",
            "2025-11-03T06:30:00.000Z",
        ]
    );
}
