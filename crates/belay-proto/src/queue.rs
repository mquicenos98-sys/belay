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

impl StateCounts {
    /// The count for `state`.
    #[must_use]
    pub fn get(&self, state: JobState) -> u64 {
        match state {
            JobState::Scheduled => self.scheduled,
            JobState::Ready => self.ready,
            JobState::Running => self.running,
            JobState::Retrying => self.retrying,
            JobState::Succeeded => self.succeeded,
            JobState::Dead => self.dead,
            JobState::Cancelled => self.cancelled,
        }
    }

    /// Mutable access to the count for `state`.
    pub fn get_mut(&mut self, state: JobState) -> &mut u64 {
        match state {
            JobState::Scheduled => &mut self.scheduled,
            JobState::Ready => &mut self.ready,
            JobState::Running => &mut self.running,
            JobState::Retrying => &mut self.retrying,
            JobState::Succeeded => &mut self.succeeded,
            JobState::Dead => &mut self.dead,
            JobState::Cancelled => &mut self.cancelled,
        }
    }

    /// Counts a new job in `state`.
    pub fn increment(&mut self, state: JobState) {
        *self.get_mut(state) += 1;
    }

    /// Stops counting a job in `state` (saturating at zero).
    pub fn decrement(&mut self, state: JobState) {
        let count = self.get_mut(state);
        *count = count.saturating_sub(1);
    }

    /// Moves one job from `from` to `to`.
    pub fn record_transition(&mut self, from: JobState, to: JobState) {
        self.decrement(from);
        self.increment(to);
    }

    /// Jobs in any state.
    #[must_use]
    pub fn total(&self) -> u64 {
        JobState::ALL.iter().map(|s| self.get(*s)).sum()
    }

    /// Jobs not yet finished.
    #[must_use]
    pub fn pending(&self) -> u64 {
        JobState::ALL
            .iter()
            .filter(|s| s.is_pending())
            .map(|s| self.get(*s))
            .sum()
    }

    /// `(state, count)` pairs in lifecycle order.
    pub fn iter(&self) -> impl Iterator<Item = (JobState, u64)> + '_ {
        JobState::ALL.into_iter().map(|s| (s, self.get(s)))
    }
}

impl Add for StateCounts {
    type Output = StateCounts;

    fn add(mut self, rhs: StateCounts) -> StateCounts {
        self += rhs;
        self
    }
}

impl AddAssign for StateCounts {
    fn add_assign(&mut self, rhs: StateCounts) {
        for state in JobState::ALL {
            *self.get_mut(state) += rhs.get(state);
        }
    }
}

impl ToJson for StateCounts {
    fn to_json(&self) -> Value {
        let mut b = ObjectBuilder::with_capacity(JobState::ALL.len());
        for (state, count) in self.iter() {
            b.insert(state.as_str(), count);
        }
        b.build()
    }
}

impl FromJson for StateCounts {
    fn from_json(value: &Value) -> Result<StateCounts, FromJsonError> {
        let r = ObjectReader::new(value, "StateCounts")?;
        Ok(StateCounts {
            scheduled: r.or_default("scheduled")?,
            ready: r.or_default("ready")?,
            running: r.or_default("running")?,
            retrying: r.or_default("retrying")?,
            succeeded: r.or_default("succeeded")?,
            dead: r.or_default("dead")?,
            cancelled: r.or_default("cancelled")?,
        })
    }
}

/// Activity over a trailing window, e.g. the last minute.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Throughput {
    /// Length of the window.
    pub window: Duration,
    /// Jobs enqueued.
    pub enqueued: u64,
    /// Jobs that succeeded.
    pub succeeded: u64,
    /// Attempts that failed (retried or not).
    pub failed: u64,
    /// Jobs that died.
    pub dead: u64,
}

impl Throughput {
    /// Successful completions per second over the window.
    #[must_use]
    pub fn succeeded_per_second(&self) -> f64 {
        let secs = self.window.as_secs_f64();
        if secs == 0.0 {
            0.0
        } else {
            self.succeeded as f64 / secs
        }
    }
}

impl ToJson for Throughput {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .duration("window", self.window)
            .field("enqueued", self.enqueued)
            .field("succeeded", self.succeeded)
            .field("failed", self.failed)
            .field("dead", self.dead)
            .build()
    }
}

impl FromJson for Throughput {
    fn from_json(value: &Value) -> Result<Throughput, FromJsonError> {
        let r = ObjectReader::new(value, "Throughput")?;
        Ok(Throughput {
            window: r.req_duration("window")?,
            enqueued: r.or_default("enqueued")?,
            succeeded: r.or_default("succeeded")?,
            failed: r.or_default("failed")?,
            dead: r.or_default("dead")?,
        })
    }
}

/// A queue's status: configuration, depth and recent activity.
#[derive(Clone, Debug, PartialEq)]
pub struct QueueInfo {
    /// The queue.
    pub name: QueueName,
    /// Whether leasing is paused (enqueue still works).
    pub paused: bool,
    /// Current configuration.
    pub config: QueueConfig,
    /// Jobs per state.
    pub counts: StateCounts,
    /// How long the oldest ready job has been eligible; `None` when nothing
    /// is ready. The best single signal that workers are falling behind.
    pub oldest_ready_age: Option<Duration>,
    /// Recent activity.
    pub throughput: Throughput,
}

impl QueueInfo {
    /// A queue with default configuration and no jobs.
    #[must_use]
    pub fn new(name: QueueName) -> QueueInfo {
        QueueInfo {
            name,
            paused: false,
            config: QueueConfig::default(),
            counts: StateCounts::default(),
            oldest_ready_age: None,
            throughput: Throughput::default(),
        }
    }
}

impl ToJson for QueueInfo {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("name", &self.name)
            .field("paused", self.paused)
            .field("config", &self.config)
            .field("counts", self.counts)
            .opt_duration("oldest_ready_age", self.oldest_ready_age)
            .field("throughput", self.throughput)
            .build()
    }
}

impl FromJson for QueueInfo {
    fn from_json(value: &Value) -> Result<QueueInfo, FromJsonError> {
        let r = ObjectReader::new(value, "QueueInfo")?;
        Ok(QueueInfo {
            name: r.req("name")?,
            paused: r.or_default("paused")?,
            config: r.or_default("config")?,
            counts: r.or_default("counts")?,
            oldest_ready_age: r.opt_duration("oldest_ready_age")?,
            throughput: r.or_default("throughput")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_json::json;

    #[test]
    fn empty_object_is_the_default_config() {
        let config = QueueConfig::from_json(&json!({})).unwrap();
        assert_eq!(config, QueueConfig::default());
        assert_eq!(
            config.to_json(),
            json!({
                "default_timeout": "30m",
                "default_retry": "exp:1s*2..1h~10%",
                "default_max_attempts": 25,
                "retention": {"succeeded": "1d", "dead": "7d", "cancelled": "1d"}
            })
        );
        assert!(config.validate().is_ok());
    }

    #[test]
    fn full_config_round_trips() {
        let config = QueueConfig {
            max_concurrency: Some(8),
            rate_limit: Some(RateLimit::new(2.5, 5)),
            default_timeout: Duration::from_secs(90),
            default_retry: "linear:1s+1s..1m".parse().unwrap(),
            default_max_attempts: 3,
            retention: Retention {
                succeeded: Duration::from_secs(3600),
                ..Retention::default()
            },
        };
        let v = config.to_json();
        assert_eq!(v["rate_limit"], json!({"per_second": 2.5, "burst": 5}));
        assert_eq!(QueueConfig::from_json(&v), Ok(config));
    }

    #[test]
    fn partial_retention_keeps_other_defaults() {
        let config = QueueConfig::from_json(&json!({"retention": {"dead": "30d"}})).unwrap();
        assert_eq!(config.retention.dead, Duration::from_secs(30 * 86_400));
        assert_eq!(config.retention.succeeded, DAY);
    }

    #[test]
    fn rate_limit_burst_defaults_to_one_seconds_worth() {
        let r = RateLimit::from_json(&json!({"per_second": 2.5})).unwrap();
        assert_eq!(r.burst, 3);
        let r = RateLimit::from_json(&json!({"per_second": 0.1})).unwrap();
        assert_eq!(r.burst, 1);
        assert_eq!(r.interval(), Duration::from_secs(10));
    }

    #[test]
    fn invalid_configs_report_every_problem() {
        let v = json!({
            "max_concurrency": 0,
            "rate_limit": {"per_second": -1, "burst": 0},
            "default_timeout": "10d",
            "default_max_attempts": 0,
        });
        let config = QueueConfig::from_json(&v).unwrap();
        let err = config.validate().unwrap_err();
        let lines: Vec<String> = err.iter().map(ToString::to_string).collect();
        assert_eq!(
            lines,
            [
                "$.max_concurrency: must be at least 1; omit it for no limit",
                "$.rate_limit.per_second: must be above 0 and at most 1000000, got -1",
                "$.rate_limit.burst: must be at least 1",
                "$.default_timeout: must be between 1s and 7d, got 10d",
                "$.default_max_attempts: must be between 1 and 1000, got 0",
            ]
        );
    }

    #[test]
    fn config_rejects_unknown_fields_with_hints() {
        let err = QueueConfig::from_json(&json!({"max_concurency": 3})).unwrap_err();
        assert_eq!(
            err.to_string(),
            r#"$: unknown field "max_concurency" in QueueConfig, did you mean "max_concurrency"?"#
        );
        let err = QueueConfig::from_json(&json!({"default_timeout": 30})).unwrap_err();
        assert_eq!(
            err.to_string(),
            r#"$.default_timeout: expected duration string such as "30s" or "1h30m", found 30"#
        );
    }

    #[test]
    fn state_counts_track_transitions() {
        let mut c = StateCounts::default();
        c.increment(JobState::Ready);
        c.increment(JobState::Ready);
        c.record_transition(JobState::Ready, JobState::Running);
        c.record_transition(JobState::Running, JobState::Succeeded);
        assert_eq!((c.ready, c.running, c.succeeded), (1, 0, 1));
        assert_eq!((c.total(), c.pending()), (2, 1));
        c.decrement(JobState::Dead);
        assert_eq!(c.dead, 0, "removal saturates");
        let sum = c + c;
        assert_eq!(sum.total(), 4);
        assert_eq!(
            c.to_json(),
            json!({"scheduled": 0, "ready": 1, "running": 0, "retrying": 0,
                   "succeeded": 1, "dead": 0, "cancelled": 0})
        );
        assert_eq!(StateCounts::from_json(&c.to_json()), Ok(c));
    }

    #[test]
    fn retention_applies_to_terminal_states_only() {
        let r = Retention::default();
        assert_eq!(r.for_state(JobState::Dead), Some(7 * DAY));
        assert_eq!(r.for_state(JobState::Running), None);
    }

    #[test]
    fn queue_info_round_trips() {
        let mut info = QueueInfo::new(QueueName::from_static("emails"));
        info.paused = true;
        info.counts.ready = 12;
        info.oldest_ready_age = Some(Duration::from_secs(95));
        info.throughput = Throughput {
            window: Duration::from_secs(60),
            enqueued: 30,
            succeeded: 120,
            failed: 2,
            dead: 0,
        };
        let v = info.to_json();
        assert_eq!(v["oldest_ready_age"], "1m35s");
        assert_eq!(QueueInfo::from_json(&v), Ok(info.clone()));
        assert_eq!(info.throughput.succeeded_per_second(), 2.0);
    }
}
