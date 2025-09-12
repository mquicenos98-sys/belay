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
