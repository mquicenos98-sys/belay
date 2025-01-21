//! Cron schedules: the submitted [`ScheduleSpec`], the stored [`Schedule`],
//! and the policies that say what happens after downtime ([`MisfirePolicy`])
//! or while a previous run is still going ([`OverlapPolicy`]).
//!
//! This crate does not parse cron expressions or time zones (that is
//! belay-cron's job, used by the scheduler); validation here only rejects
//! values that cannot possibly be right, such as a two-field expression.

mod calendar;

pub use calendar::{CalendarDate, CalendarSpec, TimeOfDay, TimeWindow, WeekdaySet};

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use belay_common::Timestamp;
use belay_json::{FromJson, FromJsonError, ObjectBuilder, ObjectReader, ToJson, Value};

use crate::error::ParseError;
use crate::ids::{JobId, ScheduleId, SLUG_RULE};
use crate::job::{JobSpec, RESERVED_META_PREFIX};
use crate::limits::{
    MAX_CRON_LEN, MAX_DESCRIPTION_BYTES, MAX_MISFIRE_CATCH_UP, MAX_SCHEDULE_JITTER,
    MAX_TIMEZONE_LEN,
};
use crate::validate::{Validate, Validator};
use crate::wire::{text_enum, text_json, BuilderExt, ReaderExt};

/// What to do about fire times missed while belay was down (or its tick loop
/// stalled). Fire times that come due while a schedule is paused are
/// skipped, not missed, so resuming never triggers a catch-up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MisfirePolicy {
    /// Drop missed fires; wait for the next one. Text form `skip`.
    Skip,
    /// Fire once now for all missed fires together. Text form `fire_once`;
    /// the default.
    #[default]
    FireOnce,
    /// Fire once per missed time, oldest first, but at most `max` times.
    /// Text form `fire_all:<max>`; plain `fire_all` means
    /// `fire_all:`[`DEFAULT_CATCH_UP`](Self::DEFAULT_CATCH_UP).
    FireAll {
        /// Most catch-up fires.
        max: u32,
    },
}

impl MisfirePolicy {
    /// Catch-up limit for a bare `fire_all`.
    pub const DEFAULT_CATCH_UP: u32 = 100;
}

impl fmt::Display for MisfirePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MisfirePolicy::Skip => f.write_str("skip"),
            MisfirePolicy::FireOnce => f.write_str("fire_once"),
            MisfirePolicy::FireAll { max } => write!(f, "fire_all:{max}"),
        }
    }
}

impl FromStr for MisfirePolicy {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<MisfirePolicy, ParseError> {
        const NAMES: &[&str] = &["skip", "fire_once", "fire_all", "fire_all:<max>"];
        match s {
            "skip" => Ok(MisfirePolicy::Skip),
            "fire_once" => Ok(MisfirePolicy::FireOnce),
            "fire_all" => Ok(MisfirePolicy::FireAll {
                max: MisfirePolicy::DEFAULT_CATCH_UP,
            }),
            _ => match s.strip_prefix("fire_all:") {
                Some(max) => max
                    .parse::<u32>()
                    .map(|max| MisfirePolicy::FireAll { max })
                    .map_err(|_| {
                        ParseError::invalid("misfire policy", s, "expected fire_all:<count>")
                    }),
                None => Err(ParseError::unknown("misfire policy", s, NAMES)),
            },
        }
    }
}

text_json!(MisfirePolicy, "misfire policy");

text_enum! {
    /// What to do when a fire time comes while the job from the previous
    /// fire has not finished.
    #[derive(Default)]
    OverlapPolicy, "overlap policy" {
        /// Fire anyway; runs may overlap (the default).
        #[default]
        Allow = "allow",
        /// Skip this fire.
        Skip = "skip",
        /// Cancel the previous job, then fire.
        CancelPrevious = "cancel_previous",
    }
}

/// A recurring job: fire `template` at every time matching `cron` in
/// `timezone`.
///
/// ```
/// use belay_json::ToJson;
/// use belay_proto::{JobSpec, OverlapPolicy, ScheduleSpec};
///
/// let spec = ScheduleSpec::new(
///     "nightly-report".parse().unwrap(),
///     "0 2 * * *",
///     JobSpec::new("reports", "build_daily"),
/// )
/// .timezone("America/Bogota")
/// .overlap(OverlapPolicy::Skip);
/// assert_eq!(spec.to_json()["overlap"], "skip");
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct ScheduleSpec {
    /// The schedule's id.
    pub id: ScheduleId,
    /// A 5, 6 or 7 field cron expression, or a nickname such as `@daily`.
    pub cron: String,
    /// IANA zone name, fixed offset or POSIX TZ string; default `UTC`.
    pub timezone: String,
    /// The job to enqueue on every fire. Its `run_at` and `unique` must be
    /// unset: the scheduler sets them per fire.
    pub template: JobSpec,
    /// Handling of missed fires.
    pub misfire: MisfirePolicy,
    /// Handling of fires while the previous job still runs.
    pub overlap: OverlapPolicy,
    /// Upper bound of a random delay added to each fire, so that many
    /// schedules on the same minute do not stampede. Deterministic per
    /// schedule and fire time.
    pub jitter: Duration,
    /// Paused schedules do not fire.
    pub paused: bool,
    /// Do not fire before this time.
    pub starts_at: Option<Timestamp>,
    /// Do not fire at or after this time.
    pub ends_at: Option<Timestamp>,
    /// Name of a [`CalendarSpec`] restricting when fires may happen.
    pub calendar: Option<String>,
    /// Free text for humans.
    pub description: Option<String>,
}

impl ScheduleSpec {
    /// Default time zone.
    pub const DEFAULT_TIMEZONE: &'static str = "UTC";

    /// A schedule with default policies in UTC.
    #[must_use]
    pub fn new(id: ScheduleId, cron: impl Into<String>, template: JobSpec) -> ScheduleSpec {
        ScheduleSpec {
            id,
            cron: cron.into(),
            timezone: ScheduleSpec::DEFAULT_TIMEZONE.to_owned(),
            template,
            misfire: MisfirePolicy::default(),
            overlap: OverlapPolicy::default(),
            jitter: Duration::ZERO,
            paused: false,
            starts_at: None,
            ends_at: None,
            calendar: None,
            description: None,
        }
    }

    /// Sets the time zone.
    #[must_use]
    pub fn timezone(mut self, tz: impl Into<String>) -> ScheduleSpec {
        self.timezone = tz.into();
        self
    }

    /// Sets the misfire policy.
    #[must_use]
    pub fn misfire(mut self, policy: MisfirePolicy) -> ScheduleSpec {
        self.misfire = policy;
        self
    }

    /// Sets the overlap policy.
    #[must_use]
    pub fn overlap(mut self, policy: OverlapPolicy) -> ScheduleSpec {
        self.overlap = policy;
        self
    }

    /// Sets the jitter bound.
    #[must_use]
    pub fn jitter(mut self, jitter: Duration) -> ScheduleSpec {
        self.jitter = jitter;
        self
    }

    /// Restricts fires to a calendar.
    #[must_use]
    pub fn calendar(mut self, name: impl Into<String>) -> ScheduleSpec {
        self.calendar = Some(name.into());
        self
    }

    /// Restricts fires to `[starts_at, ends_at)`.
    #[must_use]
    pub fn between(
        mut self,
        starts_at: Option<Timestamp>,
        ends_at: Option<Timestamp>,
    ) -> ScheduleSpec {
        self.starts_at = starts_at;
        self.ends_at = ends_at;
        self
    }

    /// Sets the description.
    #[must_use]
    pub fn description(mut self, text: impl Into<String>) -> ScheduleSpec {
        self.description = Some(text.into());
        self
    }

    /// Whether `t` lies inside the `[starts_at, ends_at)` bounds.
    #[must_use]
    pub fn within_bounds(&self, t: Timestamp) -> bool {
        self.starts_at.map_or(true, |s| t >= s) && self.ends_at.map_or(true, |e| t < e)
    }
}

impl ToJson for ScheduleSpec {
    fn to_json(&self) -> Value {
        ObjectBuilder::with_capacity(12)
            .field("id", &self.id)
            .field("cron", &self.cron)
            .field("timezone", &self.timezone)
            .field("template", &self.template)
            .field("misfire", self.misfire)
            .field("overlap", self.overlap)
            .duration("jitter", self.jitter)
            .field("paused", self.paused)
            .opt_timestamp("starts_at", self.starts_at)
            .opt_timestamp("ends_at", self.ends_at)
            .opt_field("calendar", &self.calendar)
            .opt_field("description", &self.description)
            .build()
    }
}

impl FromJson for ScheduleSpec {
    fn from_json(value: &Value) -> Result<ScheduleSpec, FromJsonError> {
        let r = ObjectReader::new(value, "ScheduleSpec")?;
        let spec = ScheduleSpec {
            id: r.req("id")?,
            cron: r.req("cron")?,
            timezone: r.or_else("timezone", || ScheduleSpec::DEFAULT_TIMEZONE.to_owned())?,
            template: r.req("template")?,
            misfire: r.or_default("misfire")?,
            overlap: r.or_default("overlap")?,
            jitter: r.duration_or("jitter", Duration::ZERO)?,
            paused: r.or_default("paused")?,
            starts_at: r.opt_timestamp("starts_at")?,
            ends_at: r.opt_timestamp("ends_at")?,
            calendar: r.opt("calendar")?,
            description: r.opt("description")?,
        };
        r.deny_unknown_fields()?;
        Ok(spec)
    }
}

impl Validate for ScheduleSpec {
    fn validate_into(&self, v: &mut Validator) {
        if let Some(problem) = cron_shape_problem(&self.cron) {
            v.report("cron", problem);
        }
        validate_timezone(v, &self.timezone);
        v.nested("template", &self.template);
        v.at_key("template", |v| {
            let t = &self.template;
            v.check(t.run_at.is_none(), "run_at", || {
                "must not be set; each fire computes its own run time".to_owned()
            });
            v.check(t.unique.is_none(), "unique", || {
                "must not be set; the scheduler gives every fire its own key".to_owned()
            });
            v.check(t.workflow.is_none(), "workflow", || {
                "must not be set on a schedule template".to_owned()
            });
            v.at_key("meta", |v| {
                for key in t
                    .meta
                    .keys()
                    .filter(|k| k.starts_with(RESERVED_META_PREFIX))
                {
                    v.report(
                        key,
                        format!("keys starting with {RESERVED_META_PREFIX:?} are set by belay"),
                    );
                }
            });
        });
        if let MisfirePolicy::FireAll { max } = self.misfire {
            v.check_range("misfire", max, 1, MAX_MISFIRE_CATCH_UP);
        }
        v.check_duration("jitter", self.jitter, Duration::ZERO, MAX_SCHEDULE_JITTER);
        if let (Some(start), Some(end)) = (self.starts_at, self.ends_at) {
            v.check(start < end, "ends_at", || {
                format!("must be after starts_at ({start})")
            });
        }
        if let Some(calendar) = &self.calendar {
            v.check_name("calendar", calendar, SLUG_RULE);
        }
        if let Some(text) = &self.description {
            v.check_bytes("description", text, MAX_DESCRIPTION_BYTES);
        }
    }
}

/// Rejects expressions no cron dialect accepts; belay-cron does the rest.
fn cron_shape_problem(cron: &str) -> Option<String> {
    let trimmed = cron.trim();
    if trimmed.is_empty() {
        return Some("must not be empty".to_owned());
    }
    if cron.len() > MAX_CRON_LEN {
        return Some(format!(
            "must be at most {MAX_CRON_LEN} bytes, got {}",
            cron.len()
        ));
    }
    if cron.chars().any(char::is_control) {
        return Some("must not contain control characters".to_owned());
    }
    let fields = trimmed.split_whitespace().count();
    if trimmed.starts_with('@') {
        (fields != 1)
            .then(|| format!("a nickname such as @daily stands alone, got {fields} fields"))
    } else {
        (!(5..=7).contains(&fields)).then(|| format!("must have 5, 6 or 7 fields, got {fields}"))
    }
}

/// Time zone strings: non-empty, short, printable, no spaces. Whether the
/// zone exists is checked by the scheduler, which owns the zone table.
pub(crate) fn validate_timezone(v: &mut Validator, tz: &str) {
    let problem = if tz.is_empty() {
        Some("must not be empty".to_owned())
    } else if tz.len() > MAX_TIMEZONE_LEN {
        Some(format!(
            "must be at most {MAX_TIMEZONE_LEN} bytes, got {}",
            tz.len()
        ))
    } else if tz.chars().any(|c| c.is_whitespace() || c.is_control()) {
        Some("must not contain spaces or control characters".to_owned())
    } else {
        None
    };
    if let Some(problem) = problem {
        v.report("timezone", problem);
    }
}

/// A schedule and its firing history.
#[derive(Clone, Debug, PartialEq)]
pub struct Schedule {
    /// What was submitted.
    pub spec: ScheduleSpec,
    /// The next time it will fire, before jitter; `None` if it never will
    /// again (past `ends_at`, or an expression with no future match).
    pub next_fire_at: Option<Timestamp>,
    /// The nominal time of the latest fire.
    pub last_fire_at: Option<Timestamp>,
    /// The job enqueued by the latest fire.
    pub last_job: Option<JobId>,
    /// Fires so far.
    pub fire_count: u64,
    /// When it was created.
    pub created_at: Timestamp,
    /// When it last changed.
    pub updated_at: Timestamp,
}

impl Schedule {
    /// A schedule that has never fired.
    #[must_use]
    pub fn new(spec: ScheduleSpec, next_fire_at: Option<Timestamp>, now: Timestamp) -> Schedule {
        Schedule {
            spec,
            next_fire_at,
            last_fire_at: None,
            last_job: None,
            fire_count: 0,
            created_at: now,
            updated_at: now,
        }
    }

    /// The schedule's id.
    #[must_use]
    pub fn id(&self) -> &ScheduleId {
        &self.spec.id
    }

    /// Records a fire at nominal time `fire_time` that enqueued `job`.
    pub fn record_fire(
        &mut self,
        fire_time: Timestamp,
        job: JobId,
        next_fire_at: Option<Timestamp>,
        now: Timestamp,
    ) {
        self.last_fire_at = Some(fire_time);
        self.last_job = Some(job);
        self.fire_count += 1;
        self.next_fire_at = next_fire_at;
        self.updated_at = now;
    }

    /// Whether a fire at `t` may happen: not paused and within bounds.
    #[must_use]
    pub fn is_active_at(&self, t: Timestamp) -> bool {
        !self.spec.paused && self.spec.within_bounds(t)
    }
}

impl ToJson for Schedule {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("spec", &self.spec)
            .opt_timestamp("next_fire_at", self.next_fire_at)
            .opt_timestamp("last_fire_at", self.last_fire_at)
            .opt_field("last_job", &self.last_job)
            .field("fire_count", self.fire_count)
            .timestamp("created_at", self.created_at)
            .timestamp("updated_at", self.updated_at)
            .build()
    }
}

impl FromJson for Schedule {
    fn from_json(value: &Value) -> Result<Schedule, FromJsonError> {
        let r = ObjectReader::new(value, "Schedule")?;
        Ok(Schedule {
            spec: r.req("spec")?,
            next_fire_at: r.opt_timestamp("next_fire_at")?,
            last_fire_at: r.opt_timestamp("last_fire_at")?,
            last_job: r.opt("last_job")?,
            fire_count: r.or_default("fire_count")?,
            created_at: r.req_timestamp("created_at")?,
            updated_at: r.req_timestamp("updated_at")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_json::json;

    fn spec() -> ScheduleSpec {
        ScheduleSpec::new(
            ScheduleId::from_static("nightly-report"),
            "0 2 * * *",
            JobSpec::new("reports", "build"),
        )
    }

    fn problems(spec: &ScheduleSpec) -> Vec<String> {
        match spec.validate() {
            Ok(()) => Vec::new(),
            Err(e) => e.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn minimal_schedule_json_fills_defaults() {
        let v = json!({
            "id": "nightly-report",
            "cron": "0 2 * * *",
            "template": {"queue": "reports", "kind": "build"}
        });
        let parsed = ScheduleSpec::from_json(&v).unwrap();
        assert_eq!(parsed, spec());
        assert_eq!(
            parsed.to_json(),
            json!({
                "id": "nightly-report", "cron": "0 2 * * *", "timezone": "UTC",
                "template": {"queue": "reports", "kind": "build", "priority": 0},
                "misfire": "fire_once", "overlap": "allow", "jitter": "0s", "paused": false
            })
        );
    }

    #[test]
    fn full_schedule_round_trips() {
        let s = spec()
            .timezone("Europe/Berlin")
            .misfire(MisfirePolicy::FireAll { max: 5 })
            .overlap(OverlapPolicy::CancelPrevious)
            .jitter(Duration::from_secs(30))
            .between(
                Some(Timestamp::from_millis(0)),
                Some(Timestamp::from_millis(86_400_000)),
            )
            .calendar("workdays")
            .description("daily numbers for finance");
        let v = s.to_json();
        assert_eq!(v["misfire"], "fire_all:5");
        assert_eq!(ScheduleSpec::from_json(&v), Ok(s.clone()));
        assert!(s.validate().is_ok());
    }

    #[test]
    fn policy_text_forms() {
        assert_eq!("fire_all".parse(), Ok(MisfirePolicy::FireAll { max: 100 }));
        assert_eq!("fire_all:3".parse(), Ok(MisfirePolicy::FireAll { max: 3 }));
        assert_eq!(
            "fire_all:x"
                .parse::<MisfirePolicy>()
                .unwrap_err()
                .to_string(),
            r#"invalid misfire policy "fire_all:x": expected fire_all:<count>"#
        );
        assert_eq!(
            "fire_onse"
                .parse::<MisfirePolicy>()
                .unwrap_err()
                .to_string(),
            r#"unknown misfire policy "fire_onse", did you mean "fire_once"?"#
        );
        for p in [
            OverlapPolicy::Allow,
            OverlapPolicy::Skip,
            OverlapPolicy::CancelPrevious,
        ] {
            assert_eq!(p.to_string().parse(), Ok(p));
        }
    }

    #[test]
    fn rejects_templates_the_scheduler_must_control() {
        let mut s = spec();
        s.template = s
            .template
            .run_at(Timestamp::from_millis(5))
            .unique("x", crate::UniqueScope::UntilFinished)
            .meta(crate::job::META_SCHEDULE, "spoofed");
        assert_eq!(
            problems(&s),
            [
                "$.template.run_at: must not be set; each fire computes its own run time",
                "$.template.unique: must not be set; the scheduler gives every fire its own key",
                r#"$.template.meta["belay.schedule"]: keys starting with "belay." are set by belay"#,
            ]
        );
    }

    #[test]
    fn validation_catches_impossible_values() {
        let mut s = spec()
            .timezone("Europe/ Berlin")
            .jitter(Duration::from_secs(7200));
        s.cron = "0 2 *".into();
        s.misfire = MisfirePolicy::FireAll { max: 0 };
        s.starts_at = Some(Timestamp::from_millis(10));
        s.ends_at = Some(Timestamp::from_millis(10));
        s.calendar = Some("Work Days".into());
        s.template.priority = 2000;
        assert_eq!(
            problems(&s),
            [
                "$.cron: must have 5, 6 or 7 fields, got 3",
                "$.timezone: must not contain spaces or control characters",
                "$.template.priority: must be between -1000 and 1000, got 2000",
                "$.misfire: must be between 1 and 1000, got 0",
                "$.jitter: must be at most 1h, got 2h",
                "$.ends_at: must be after starts_at (1970-01-01T00:00:00.010Z)",
                "$.calendar: must start with a lowercase letter or digit, got 'W'",
            ]
        );
    }

    #[test]
    fn cron_nicknames_stand_alone() {
        assert_eq!(cron_shape_problem("@daily"), None);
        assert_eq!(cron_shape_problem(" 0 0 * * * * * "), None);
        assert_eq!(
            cron_shape_problem("@daily 5"),
            Some("a nickname such as @daily stands alone, got 2 fields".to_owned())
        );
        assert_eq!(
            cron_shape_problem("  "),
            Some("must not be empty".to_owned())
        );
    }

    #[test]
    fn bounds_are_half_open() {
        let s = spec().between(
            Some(Timestamp::from_millis(100)),
            Some(Timestamp::from_millis(200)),
        );
        assert!(!s.within_bounds(Timestamp::from_millis(99)));
        assert!(s.within_bounds(Timestamp::from_millis(100)));
        assert!(!s.within_bounds(Timestamp::from_millis(200)));
    }

    #[test]
    fn schedule_records_fires_and_round_trips() {
        let now = Timestamp::from_millis(1_000);
        let mut sched = Schedule::new(spec(), Some(Timestamp::from_millis(7_200_000)), now);
        assert!(sched.is_active_at(now));
        let job = JobId::generate();
        sched.record_fire(
            Timestamp::from_millis(7_200_000),
            job,
            Some(Timestamp::from_millis(93_600_000)),
            Timestamp::from_millis(7_200_050),
        );
        assert_eq!((sched.fire_count, sched.last_job), (1, Some(job)));
        assert_eq!(Schedule::from_json(&sched.to_json()), Ok(sched.clone()));
        sched.spec.paused = true;
        assert!(!sched.is_active_at(now));
        assert_eq!(sched.id().as_str(), "nightly-report");
    }
}
