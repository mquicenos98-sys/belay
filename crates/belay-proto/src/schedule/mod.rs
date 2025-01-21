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
