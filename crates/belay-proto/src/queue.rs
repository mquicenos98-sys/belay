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
