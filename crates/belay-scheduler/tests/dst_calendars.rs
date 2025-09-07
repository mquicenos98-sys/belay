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

#[test]
fn periodic_schedules_fire_in_both_passes_of_the_repeated_hour() {
    let h = Harness::new("2025-11-02T04:59:00Z");
    h.scheduler
        .create(spec("half-hourly", "*/30 * * * *").timezone("America/New_York"))
        .unwrap();
    let reports = h.run_until("2025-11-02T07:01:00Z");
    // 01:00 and 01:30 EDT, then 01:00 and 01:30 EST, then 02:00 EST.
    assert_eq!(
        fired(&reports),
        [
            "2025-11-02T05:00:00.000Z",
            "2025-11-02T05:30:00.000Z",
            "2025-11-02T06:00:00.000Z",
            "2025-11-02T06:30:00.000Z",
            "2025-11-02T07:00:00.000Z",
        ]
    );
}

#[test]
fn catching_up_across_the_gap_uses_the_shifted_fire_time() {
    let h = Harness::new("2025-03-08T12:00:00Z");
    h.scheduler
        .create(
            spec("nightly", "30 2 * * *")
                .timezone("America/New_York")
                .misfire(MisfirePolicy::FireAll { max: 10 }),
        )
        .unwrap();
    let report = h.tick_at("2025-03-10T12:00:00Z");
    let times: Vec<String> = report
        .fired
        .iter()
        .map(|f| f.fire_time.to_string())
        .collect();
    assert_eq!(
        times,
        ["2025-03-09T07:00:00.000Z", "2025-03-10T06:30:00.000Z"]
    );
    assert!(report.fired.iter().all(|f| f.catch_up));
}

/// Weekdays 09:00-17:00 in Bogota (UTC-5 all year), closed on Christmas
/// and New Year's Day.
fn office() -> CalendarSpec {
    belay_json::from_str(
        r#"{"name": "office", "timezone": "America/Bogota",
            "exclude_dates": ["2025-12-25", "2026-01-01"],
            "windows": [{"days": "mon-fri", "start": "09:00", "end": "17:00"}],
            "description": "Bogota office hours"}"#,
    )
    .unwrap()
}

#[test]
fn business_hours_and_holidays_limit_an_hourly_schedule() {
    // Wednesday 2025-12-24, 07:00 in Bogota.
    let h = Harness::new("2025-12-24T12:00:00Z");
    h.scheduler.put_calendar(office()).unwrap();
    let created = h
        .scheduler
        .create(spec("sync", "0 * * * *").calendar("office"))
        .unwrap();
    // 13:00Z is 08:00 local, before opening.
    assert_eq!(created.next_fire_at, Some(ts("2025-12-24T14:00:00Z")));
    h.events();

    let reports = h.run_until("2025-12-29T23:00:00Z");
    let times = fired(&reports);
    // Wednesday, Friday and Monday, 09:00 through 16:00 local: Christmas
    // Thursday and the weekend are closed.
    assert_eq!(times.len(), 24);
    assert_eq!(times[0], "2025-12-24T14:00:00.000Z");
    assert_eq!(times[7], "2025-12-24T21:00:00.000Z");
    assert_eq!(times[8], "2025-12-26T14:00:00.000Z");
    assert_eq!(times[16], "2025-12-29T14:00:00.000Z");

    // Each closed stretch is reported once, when the scheduler moves past
    // it, with its exact edges.
    let skips = skipped(&reports);
    let summary: Vec<(SkipReason, String, String, u32)> = skips
        .iter()
        .map(|s| {
            (
                s.reason,
                s.fire_time.to_string(),
                s.last_fire_time.to_string(),
                s.count,
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (
                SkipReason::Calendar,
                "2025-12-24T22:00:00.000Z".to_owned(),
                "2025-12-26T13:00:00.000Z".to_owned(),
                40
            ),
            (
                SkipReason::Calendar,
                "2025-12-26T22:00:00.000Z".to_owned(),
                "2025-12-29T13:00:00.000Z".to_owned(),
                64
            ),
            (
                SkipReason::Calendar,
                "2025-12-29T22:00:00.000Z".to_owned(),
                "2025-12-30T13:00:00.000Z".to_owned(),
                16
            ),
        ]
    );
    let calendar_events: Vec<EventKind> = h
        .sink
        .events()
        .into_iter()
        .filter(|e| e.type_name() == "schedule.skipped")
        .collect();
    assert_eq!(calendar_events.len(), 3);
    assert!(matches!(
        &calendar_events[0],
        EventKind::ScheduleSkipped { reason: EventSkipReason::Calendar, fire_time, .. }
            if *fire_time == ts("2025-12-24T22:00:00Z")
    ));
}

#[test]
fn a_calendar_is_read_in_its_own_zone() {
    let h = Harness::new("2025-12-22T00:00:00Z");
    h.scheduler.put_calendar(office()).unwrap();
    // 15:00 in Madrid (UTC+1 in winter) is 09:00 in Bogota: open on
    // weekdays. 23:00 in Madrid is 17:00 in Bogota: closed.
    h.scheduler
        .create(
            spec("eu-sync", "0 15,23 * * *")
                .timezone("Europe/Madrid")
                .calendar("office"),
        )
        .unwrap();
    let times: Vec<String> = h
        .scheduler
        .preview(&id("eu-sync"), 5)
        .unwrap()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        times,
        [
            "2025-12-22T14:00:00.000Z",
            "2025-12-23T14:00:00.000Z",
            "2025-12-24T14:00:00.000Z",
            "2025-12-26T14:00:00.000Z",
            "2025-12-29T14:00:00.000Z",
        ]
    );
}

#[test]
fn a_lunch_break_splits_the_business_day() {
    let h = Harness::new("2025-12-22T13:00:00Z");
    let mut split = office();
    split.name = "split".into();
    split.windows = belay_json::from_str(
        r#"[{"days": "mon-fri", "start": "09:00", "end": "12:00"},
            {"days": "mon-fri", "start": "14:00", "end": "17:00"}]"#,
    )
    .unwrap();
    h.scheduler.put_calendar(split).unwrap();
    h.scheduler
        .create(
            spec("poll", "*/30 * * * *")
                .timezone("America/Bogota")
                .calendar("split"),
        )
        .unwrap();
    let local: Vec<String> = h
        .scheduler
        .preview(&id("poll"), 13)
        .unwrap()
        .iter()
        // Bogota is UTC-5 all year; print local wall-clock times.
        .map(|t| {
            let local = *t - std::time::Duration::from_secs(5 * 3600);
            local.to_string()[..16].to_owned()
        })
        .collect();
    assert_eq!(
        local,
        [
            "2025-12-22T09:00",
            "2025-12-22T09:30",
            "2025-12-22T10:00",
            "2025-12-22T10:30",
            "2025-12-22T11:00",
            "2025-12-22T11:30",
            "2025-12-22T14:00",
            "2025-12-22T14:30",
            "2025-12-22T15:00",
            "2025-12-22T15:30",
            "2025-12-22T16:00",
            "2025-12-22T16:30",
            "2025-12-23T09:00",
        ]
    );
}
