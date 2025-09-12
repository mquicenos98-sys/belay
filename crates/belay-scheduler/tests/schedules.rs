//! Managing schedules and calendars: creation, validation, updates,
//! pause/resume, previews, wakeups and the rules tying calendars to the
//! schedules that use them.

mod support;

use belay_json::ToJson;
use belay_proto::api::ApiError;
use belay_proto::{CalendarSpec, JobSpec, MisfirePolicy};
use belay_scheduler::{Error, Upserted};
use belay_store::Kv;

use support::{id, spec, ts, Harness};

fn problems(err: Error) -> Vec<String> {
    err.violations()
        .unwrap_or_else(|| panic!("expected a validation error, got {err}"))
        .iter()
        .map(ToString::to_string)
        .collect()
}

fn office() -> CalendarSpec {
    belay_json::from_str(
        r#"{"name": "office", "timezone": "America/Bogota",
            "windows": [{"days": "mon-fri", "start": "09:00", "end": "17:00"}]}"#,
    )
    .unwrap()
}

#[test]
fn create_computes_the_first_fire_and_announces_it() {
    let h = Harness::new("2025-06-02T10:07:00Z");
    let created = h.scheduler.create(spec("sync", "*/15 * * * *")).unwrap();
    assert_eq!(created.next_fire_at, Some(ts("2025-06-02T10:15:00Z")));
    assert_eq!(created.fire_count, 0);
    assert_eq!(created.created_at, ts("2025-06-02T10:07:00Z"));
    assert_eq!(h.scheduler.get(&id("sync")), Some(created));
    assert_eq!(h.events(), ["schedule.created"]);
    assert_eq!(h.scheduler.len(), 1);
}

#[test]
fn create_refuses_a_taken_id() {
    let h = Harness::new("2025-06-02T10:07:00Z");
    h.scheduler.create(spec("sync", "@hourly")).unwrap();
    let err = h.scheduler.create(spec("sync", "@daily")).unwrap_err();
    assert_eq!(err.to_string(), r#"schedule "sync" already exists"#);
    assert_eq!(
        h.scheduler.get(&id("sync")).unwrap().spec.cron,
        "@hourly",
        "the original is untouched"
    );
}

#[test]
fn invalid_specs_report_every_problem_with_its_path() {
    let h = Harness::new("2025-06-02T10:07:00Z");
    let mut workdays = office();
    workdays.name = "workdays".into();
    h.scheduler.put_calendar(workdays).unwrap();
    let mut bad = spec("report", "61 * * * *")
        .timezone("Europe/Berln")
        .calendar("workdayz");
    bad.template = JobSpec::new("jobs", "work").run_at(ts("2025-06-03T00:00:00Z"));
    let err = h.scheduler.create(bad).unwrap_err();
    assert_eq!(
        problems(err),
        [
            "$.cron: invalid value 61 in minute field (0-59) at column 1",
            r#"$.timezone: unknown time zone "Europe/Berln" (did you mean "Europe/Berlin"?)"#,
            "$.template.run_at: must not be set; each fire computes its own run time",
            r#"$.calendar: unknown calendar "workdayz", did you mean "workdays"?"#,
        ]
    );
    assert!(h.scheduler.is_empty());
    assert!(h.events().is_empty());
}

#[test]
fn impossible_expressions_are_rejected_up_front() {
    let h = Harness::new("2025-06-02T10:07:00Z");
    let err = h.scheduler.create(spec("never", "0 0 30 2 *")).unwrap_err();
    assert_eq!(
        problems(err),
        [r#"$.cron: "0 0 30 2 *" can never match (no date satisfies its day and month fields)"#]
    );
    let err = h.scheduler.create(spec("short", "0 2 *")).unwrap_err();
    assert_eq!(
        problems(err),
        ["$.cron: expected 5, 6 or 7 fields, found 3"]
    );
}

#[test]
fn a_cron_that_never_meets_its_calendar_is_rejected() {
    let h = Harness::new("2025-06-02T10:07:00Z");
    h.scheduler.put_calendar(office()).unwrap();
    // 03:00 UTC is 22:00 in Bogota, always outside office hours.
    let err = h
        .scheduler
        .create(spec("night", "0 3 * * *").calendar("office"))
        .unwrap_err();
    assert_eq!(
        problems(err),
        [r#"$.calendar: no time matching the cron expression is allowed by calendar "office""#]
    );
}

#[test]
fn api_errors_carry_violations_as_details() {
    let h = Harness::new("2025-06-02T10:07:00Z");
    let err = h
        .scheduler
        .create(spec("bad", "* * * * *").timezone("Mars/Olympus"))
        .unwrap_err();
    let api = ApiError::from(err);
    assert_eq!(api.http_status(), 400);
    assert_eq!(api.details.unwrap()["violations"][0]["field"], "$.timezone");
    let api = ApiError::from(h.scheduler.delete(&id("ghost")).unwrap_err());
    assert_eq!(api.http_status(), 404);
}

#[test]
fn update_keeps_history_and_recomputes_the_next_fire() {
    let h = Harness::new("2025-06-02T10:07:00Z");
    h.scheduler.create(spec("sync", "*/15 * * * *")).unwrap();
    assert_eq!(h.tick_at("2025-06-02T10:15:00Z").fired.len(), 1);
    let fired = h.scheduler.get(&id("sync")).unwrap();
    assert_eq!(fired.fire_count, 1);
    h.events();

    h.clock.set(ts("2025-06-02T10:20:00Z"));
    let updated = h.scheduler.update(spec("sync", "0 * * * *")).unwrap();
    assert_eq!(updated.fire_count, 1);
    assert_eq!(updated.last_job, fired.last_job);
    assert_eq!(updated.created_at, fired.created_at);
    assert_eq!(updated.updated_at, ts("2025-06-02T10:20:00Z"));
    assert_eq!(updated.next_fire_at, Some(ts("2025-06-02T11:00:00Z")));
    assert_eq!(h.events(), ["schedule.updated"]);

    // Replacing a definition with itself is not a change.
    assert_eq!(
        h.scheduler.update(spec("sync", "0 * * * *")).unwrap(),
        updated
    );
    assert!(h.events().is_empty());
    assert!(h
        .scheduler
        .update(spec("ghost", "@daily"))
        .unwrap_err()
        .is_not_found());
}

#[test]
fn upsert_reports_what_it_did() {
    let h = Harness::new("2025-06-02T10:07:00Z");
    let (_, what) = h.scheduler.upsert(spec("cfg", "@daily")).unwrap();
    assert_eq!(what, Upserted::Created);
    let (_, what) = h.scheduler.upsert(spec("cfg", "@daily")).unwrap();
    assert_eq!(what, Upserted::Unchanged);
    let changed = spec("cfg", "@daily").misfire(MisfirePolicy::Skip);
    let (schedule, what) = h.scheduler.upsert(changed).unwrap();
    assert_eq!(what, Upserted::Updated);
    assert_eq!(schedule.spec.misfire, MisfirePolicy::Skip);
    assert_eq!(h.events(), ["schedule.created", "schedule.updated"]);
}

#[test]
fn delete_removes_the_schedule_and_its_record() {
    let h = Harness::new("2025-06-02T10:07:00Z");
    h.scheduler.create(spec("sync", "@hourly")).unwrap();
    assert!(h.kv.get(b"sched/sync").unwrap().is_some());
    let gone = h.scheduler.delete(&id("sync")).unwrap();
    assert_eq!(gone.spec.cron, "@hourly");
    assert!(h.kv.get(b"sched/sync").unwrap().is_none());
    assert_eq!(h.scheduler.get(&id("sync")), None);
    assert_eq!(h.events(), ["schedule.created", "schedule.deleted"]);
    assert!(h.scheduler.delete(&id("sync")).unwrap_err().is_not_found());
    assert_eq!(h.scheduler.next_wakeup(), None);
}

#[test]
fn pause_and_resume_are_idempotent() {
    let h = Harness::new("2025-06-02T10:07:00Z");
    h.scheduler.create(spec("sync", "@hourly")).unwrap();
    assert!(h.scheduler.pause(&id("sync")).unwrap().spec.paused);
    assert!(h.scheduler.pause(&id("sync")).unwrap().spec.paused);
    assert!(!h.scheduler.resume(&id("sync")).unwrap().spec.paused);
    assert!(!h.scheduler.resume(&id("sync")).unwrap().spec.paused);
    assert_eq!(
        h.events(),
        ["schedule.created", "schedule.paused", "schedule.resumed"]
    );
    assert!(h.scheduler.pause(&id("ghost")).unwrap_err().is_not_found());
}

#[test]
fn preview_follows_the_zone_and_starts_at_the_stored_next_fire() {
    let h = Harness::new("2025-06-02T10:07:00Z");
    h.scheduler
        .create(spec("standup", "30 9 * * MON-FRI").timezone("America/New_York"))
        .unwrap();
    let times: Vec<String> = h
        .scheduler
        .preview(&id("standup"), 4)
        .unwrap()
        .iter()
        .map(ToString::to_string)
        .collect();
    // 09:30 EDT is 13:30 UTC; Friday is followed by Monday.
    assert_eq!(
        times,
        [
            "2025-06-02T13:30:00.000Z",
            "2025-06-03T13:30:00.000Z",
            "2025-06-04T13:30:00.000Z",
            "2025-06-05T13:30:00.000Z"
        ]
    );
    assert_eq!(h.scheduler.preview(&id("standup"), 0).unwrap(), []);
    assert_eq!(
        h.scheduler.preview(&id("standup"), 5000).unwrap().len(),
        belay_scheduler::MAX_PREVIEW
    );
}

#[test]
fn preview_spec_validates_and_starts_after_the_given_time() {
    let h = Harness::new("2025-06-02T10:07:00Z");
    let times = h
        .scheduler
        .preview_spec(&spec("draft", "0 12 1 * *"), ts("2025-01-15T00:00:00Z"), 3)
        .unwrap();
    assert_eq!(
        times,
        [
            ts("2025-02-01T12:00:00Z"),
            ts("2025-03-01T12:00:00Z"),
            ts("2025-04-01T12:00:00Z")
        ]
    );
    assert!(h
        .scheduler
        .preview_spec(&spec("draft", "0 12 32 * *"), ts("2025-01-15T00:00:00Z"), 3)
        .is_err());
    assert!(h.scheduler.is_empty(), "previews store nothing");
    assert!(h.scheduler.validate(&spec("draft", "@weekly")).is_ok());
    assert!(h.scheduler.is_empty(), "validation stores nothing");
}

#[test]
fn bounds_shape_the_first_and_last_fires() {
    let h = Harness::new("2025-06-02T10:07:00Z");
    let later = spec("later", "0 * * * *").between(Some(ts("2025-06-05T00:00:00Z")), None);
    assert_eq!(
        h.scheduler.create(later).unwrap().next_fire_at,
        Some(ts("2025-06-05T00:00:00Z"))
    );
    let over = spec("over", "0 * * * *").between(
        Some(ts("2025-06-01T00:00:00Z")),
        Some(ts("2025-06-02T00:00:00Z")),
    );
    assert_eq!(h.scheduler.create(over).unwrap().next_fire_at, None);
    assert_eq!(
        h.scheduler.next_wakeup(),
        Some(ts("2025-06-05T00:00:00Z")),
        "a finished schedule never wakes the scheduler"
    );
    assert_eq!(h.scheduler.preview(&id("over"), 3).unwrap(), []);
}

#[test]
fn next_wakeup_is_the_earliest_fire_of_any_schedule() {
    let h = Harness::new("2025-06-02T10:07:00Z");
    assert_eq!(h.scheduler.next_wakeup(), None);
    h.scheduler.create(spec("hourly", "0 * * * *")).unwrap();
    h.scheduler.create(spec("quarter", "*/15 * * * *")).unwrap();
    assert_eq!(h.scheduler.next_wakeup(), Some(ts("2025-06-02T10:15:00Z")));
    h.scheduler.delete(&id("quarter")).unwrap();
    assert_eq!(h.scheduler.next_wakeup(), Some(ts("2025-06-02T11:00:00Z")));
}

#[test]
fn schedules_are_listed_and_paged_by_id() {
    let h = Harness::new("2025-06-02T10:07:00Z");
    for name in ["delta", "alpha", "charlie", "bravo"] {
        h.scheduler.create(spec(name, "@daily")).unwrap();
    }
    let names = |items: &[belay_proto::Schedule]| -> Vec<String> {
        items.iter().map(|s| s.id().to_string()).collect()
    };
    assert_eq!(
        names(&h.scheduler.list()),
        ["alpha", "bravo", "charlie", "delta"]
    );
    let first = h.scheduler.list_page(None, 3);
    assert_eq!(names(&first.items), ["alpha", "bravo", "charlie"]);
    let second = h.scheduler.list_page(first.next_cursor.as_ref(), 3);
    assert_eq!(names(&second.items), ["delta"]);
    assert!(!second.has_more());
}

#[test]
fn calendars_are_created_replaced_and_listed() {
    let h = Harness::new("2025-06-02T10:07:00Z");
    assert_eq!(
        h.scheduler.put_calendar(office()).unwrap(),
        Upserted::Created
    );
    assert_eq!(
        h.scheduler.put_calendar(office()).unwrap(),
        Upserted::Unchanged
    );
    let mut holidays = office();
    holidays.exclude_dates.insert("2025-12-25".parse().unwrap());
    assert_eq!(
        h.scheduler.put_calendar(holidays.clone()).unwrap(),
        Upserted::Updated
    );
    assert_eq!(h.scheduler.get_calendar("office"), Some(holidays.clone()));
    assert_eq!(h.scheduler.list_calendars(), [holidays.clone()]);
    assert_eq!(
        h.kv.get(b"cal/office").unwrap(),
        Some(belay_json::to_string(&holidays.to_json()).into_bytes())
    );
    assert_eq!(h.scheduler.get_calendar("nope"), None);
}

#[test]
fn a_calendar_in_use_cannot_be_deleted() {
    let h = Harness::new("2025-06-02T10:07:00Z");
    h.scheduler.put_calendar(office()).unwrap();
    h.scheduler
        .create(
            spec("standup", "30 9 * * *")
                .timezone("America/Bogota")
                .calendar("office"),
        )
        .unwrap();
    h.scheduler
        .create(
            spec("close", "0 16 * * *")
                .timezone("America/Bogota")
                .calendar("office"),
        )
        .unwrap();
    assert_eq!(
        h.scheduler.schedules_using("office"),
        [id("close"), id("standup")]
    );
    let err = h.scheduler.delete_calendar("office").unwrap_err();
    assert_eq!(
        err.to_string(),
        r#"calendar "office" is used by 2 schedules (close, standup)"#
    );
    assert!(matches!(err, Error::CalendarInUse { .. }));
    h.scheduler.delete(&id("close")).unwrap();
    h.scheduler.delete(&id("standup")).unwrap();
    assert_eq!(h.scheduler.delete_calendar("office").unwrap(), office());
    assert!(h.kv.get(b"cal/office").unwrap().is_none());
    assert!(h
        .scheduler
        .delete_calendar("office")
        .unwrap_err()
        .is_not_found());
}

#[test]
fn invalid_calendars_are_rejected() {
    let h = Harness::new("2025-06-02T10:07:00Z");
    let mut bad = office();
    bad.name = "Office Hours".into();
    bad.timezone = "America/Bogata".into();
    bad.exclude_weekdays = belay_proto::schedule::WeekdaySet::ALL;
    let err = h.scheduler.put_calendar(bad).unwrap_err();
    assert_eq!(
        problems(err),
        [
            "$.name: must start with a lowercase letter or digit, got 'O'",
            "$.exclude_weekdays: excludes every day, so nothing could ever fire",
            r#"$.timezone: unknown time zone "America/Bogata" (did you mean "America/Bogota"?)"#,
        ]
    );
    assert!(h.scheduler.list_calendars().is_empty());
}

#[test]
fn replacing_a_calendar_moves_the_fires_of_its_schedules() {
    let h = Harness::new("2025-12-23T12:00:00Z");
    h.scheduler.put_calendar(office()).unwrap();
    let created = h
        .scheduler
        .create(
            spec("standup", "30 9 * * *")
                .timezone("America/Bogota")
                .calendar("office"),
        )
        .unwrap();
    // Tuesday 07:00 in Bogota: today's 09:30 is next.
    assert_eq!(created.next_fire_at, Some(ts("2025-12-23T14:30:00Z")));
    let mut closed = office();
    closed.exclude_dates.insert("2025-12-23".parse().unwrap());
    closed.exclude_dates.insert("2025-12-24".parse().unwrap());
    h.scheduler.put_calendar(closed).unwrap();
    let moved = h.scheduler.get(&id("standup")).unwrap();
    assert_eq!(moved.next_fire_at, Some(ts("2025-12-25T14:30:00Z")));
    assert_eq!(moved.updated_at, h.now());
    // The stored copy moved in the same write.
    let stored: belay_proto::Schedule =
        belay_json::from_bytes(&h.kv.get(b"sched/standup").unwrap().unwrap()).unwrap();
    assert_eq!(stored, moved);
}
