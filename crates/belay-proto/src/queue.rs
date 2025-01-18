//! Queue configuration ([`QueueConfig`]) and the status report served by
//! `GET /v1/queues/:name` ([`QueueInfo`]).
//!
//! Queues come into existence on first enqueue with the default
//! configuration; `PUT /v1/queues/:name/config` replaces it. Every field of a
//! configuration is optional in JSON and falls back to its default, so
//! `{}` is the default configuration.

use std::ops::{Add, AddAssign};
use std::time::Duration;

use belay_common::BackoffPolicy;
use belay_json::{FromJson, FromJsonError, ObjectBuilder, ObjectReader, ToJson, Value};

use crate::ids::QueueName;
use crate::job::JobState;
use crate::limits::{
    DEFAULT_MAX_ATTEMPTS, DEFAULT_TIMEOUT, MAX_ATTEMPTS, MAX_TIMEOUT, MIN_TIMEOUT,
};
use crate::validate::{Validate, Validator};
use crate::wire::{backoff_from_json, backoff_to_json, BuilderExt, ReaderExt};

const DAY: Duration = Duration::from_secs(86_400);

/// Retention may not exceed a year: terminal jobs live in memory.
const MAX_RETENTION: Duration = Duration::from_secs(365 * 86_400);

/// How a queue treats its jobs.
#[derive(Clone, Debug, PartialEq)]
pub struct QueueConfig {
    /// Most jobs running at once across all workers; `None` for no limit.
    pub max_concurrency: Option<u32>,
    /// Most lease grants per second; `None` for no limit.
    pub rate_limit: Option<RateLimit>,
    /// Execution budget for jobs that do not set `timeout`.
    pub default_timeout: Duration,
    /// Backoff for jobs that do not set `retry`.
    pub default_retry: BackoffPolicy,
    /// Attempts for jobs that do not set `max_attempts`.
    pub default_max_attempts: u32,
    /// How long finished jobs are kept.
    pub retention: Retention,
}

impl Default for QueueConfig {
    fn default() -> QueueConfig {
        QueueConfig {
            max_concurrency: None,
            rate_limit: None,
            default_timeout: DEFAULT_TIMEOUT,
            default_retry: BackoffPolicy::default(),
            default_max_attempts: DEFAULT_MAX_ATTEMPTS,
            retention: Retention::default(),
        }
    }
}

impl ToJson for QueueConfig {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .opt_field("max_concurrency", &self.max_concurrency)
            .opt_field("rate_limit", &self.rate_limit)
            .duration("default_timeout", self.default_timeout)
            .raw("default_retry", backoff_to_json(&self.default_retry))
            .field("default_max_attempts", self.default_max_attempts)
            .field("retention", &self.retention)
            .build()
    }
}

impl FromJson for QueueConfig {
    fn from_json(value: &Value) -> Result<QueueConfig, FromJsonError> {
        let defaults = QueueConfig::default();
        let r = ObjectReader::new(value, "QueueConfig")?;
        let config = QueueConfig {
            max_concurrency: r.opt("max_concurrency")?,
            rate_limit: r.opt("rate_limit")?,
            default_timeout: r.duration_or("default_timeout", defaults.default_timeout)?,
            default_retry: r
                .opt_with("default_retry", backoff_from_json)?
                .unwrap_or(defaults.default_retry),
            default_max_attempts: r.or("default_max_attempts", defaults.default_max_attempts)?,
            retention: r.or_default("retention")?,
        };
        r.deny_unknown_fields()?;
        Ok(config)
    }
}

impl Validate for QueueConfig {
    fn validate_into(&self, v: &mut Validator) {
        if let Some(n) = self.max_concurrency {
            v.check(n >= 1, "max_concurrency", || {
                "must be at least 1; omit it for no limit".to_owned()
            });
        }
        if let Some(rate) = &self.rate_limit {
            v.nested("rate_limit", rate);
        }
        v.check_duration(
            "default_timeout",
            self.default_timeout,
            MIN_TIMEOUT,
            MAX_TIMEOUT,
        );
        v.check_range(
            "default_max_attempts",
            self.default_max_attempts,
            1,
            MAX_ATTEMPTS,
        );
        v.nested("retention", &self.retention);
    }
}

/// A token bucket: `burst` tokens, refilled at `per_second`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RateLimit {
    /// Sustained grants per second; may be fractional (`0.5` = one every
    /// two seconds).
    pub per_second: f64,
    /// Bucket size: grants allowed at once after an idle period.
    pub burst: u32,
}

impl RateLimit {
    /// Largest `per_second` accepted.
    pub const MAX_PER_SECOND: f64 = 1_000_000.0;

    /// A limit of `per_second` with a bucket of `burst`.
    #[must_use]
    pub fn new(per_second: f64, burst: u32) -> RateLimit {
        RateLimit { per_second, burst }
    }

    /// Time for one token to refill.
    #[must_use]
    pub fn interval(&self) -> Duration {
        Duration::try_from_secs_f64(1.0 / self.per_second).unwrap_or(Duration::MAX)
    }
}

impl ToJson for RateLimit {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("per_second", self.per_second)
            .field("burst", self.burst)
            .build()
    }
}

impl FromJson for RateLimit {
    fn from_json(value: &Value) -> Result<RateLimit, FromJsonError> {
        let r = ObjectReader::new(value, "RateLimit")?;
        let per_second: f64 = r.req("per_second")?;
        // A burst below one would never let anything through; default to
        // roughly one second's worth.
        let default_burst = per_second.ceil().clamp(1.0, f64::from(u32::MAX)) as u32;
        let limit = RateLimit {
            per_second,
            burst: r.or("burst", default_burst)?,
        };
        r.deny_unknown_fields()?;
        Ok(limit)
    }
}

impl Validate for RateLimit {
    fn validate_into(&self, v: &mut Validator) {
        let p = self.per_second;
        v.check(
            p > 0.0 && p <= RateLimit::MAX_PER_SECOND,
            "per_second",
            || {
                format!(
                    "must be above 0 and at most {}, got {p}",
                    RateLimit::MAX_PER_SECOND
                )
            },
        );
        v.check(self.burst >= 1, "burst", || "must be at least 1".to_owned());
    }
}

/// How long jobs are kept after finishing, per terminal state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retention {
    /// Succeeded jobs; default one day.
    pub succeeded: Duration,
    /// Dead jobs; default seven days, since someone may want to retry them.
    pub dead: Duration,
    /// Cancelled jobs; default one day.
    pub cancelled: Duration,
}

impl Retention {
    /// The retention period of a terminal `state`; `None` for others.
    #[must_use]
    pub fn for_state(&self, state: JobState) -> Option<Duration> {
        match state {
            JobState::Succeeded => Some(self.succeeded),
            JobState::Dead => Some(self.dead),
            JobState::Cancelled => Some(self.cancelled),
            _ => None,
        }
    }
}

impl Default for Retention {
    fn default() -> Retention {
        Retention {
            succeeded: DAY,
            dead: 7 * DAY,
            cancelled: DAY,
        }
    }
}

impl ToJson for Retention {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .duration("succeeded", self.succeeded)
            .duration("dead", self.dead)
            .duration("cancelled", self.cancelled)
            .build()
    }
}

impl FromJson for Retention {
    fn from_json(value: &Value) -> Result<Retention, FromJsonError> {
        let d = Retention::default();
        let r = ObjectReader::new(value, "Retention")?;
        let retention = Retention {
            succeeded: r.duration_or("succeeded", d.succeeded)?,
            dead: r.duration_or("dead", d.dead)?,
            cancelled: r.duration_or("cancelled", d.cancelled)?,
        };
        r.deny_unknown_fields()?;
        Ok(retention)
    }
}

impl Validate for Retention {
    fn validate_into(&self, v: &mut Validator) {
        v.check_duration("succeeded", self.succeeded, Duration::ZERO, MAX_RETENTION);
        v.check_duration("dead", self.dead, Duration::ZERO, MAX_RETENTION);
        v.check_duration("cancelled", self.cancelled, Duration::ZERO, MAX_RETENTION);
    }
}

/// Number of jobs in each state. JSON is an object with all seven states.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StateCounts {
    /// Jobs in `scheduled`.
    pub scheduled: u64,
    /// Jobs in `ready`.
    pub ready: u64,
    /// Jobs in `running`.
    pub running: u64,
    /// Jobs in `retrying`.
    pub retrying: u64,
    /// Jobs in `succeeded`.
    pub succeeded: u64,
    /// Jobs in `dead`.
    pub dead: u64,
    /// Jobs in `cancelled`.
    pub cancelled: u64,
}
