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
