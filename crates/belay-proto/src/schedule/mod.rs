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
