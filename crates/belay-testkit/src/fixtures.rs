//! Builders for what tests submit: jobs, schedules, workflows and their
//! payloads, with defaults chosen so that tests on a manual clock can
//! predict every retry time.
//!
//! ```
//! use belay_testkit::fixtures::{job, numbered, placeholder, step, workflow};
//! use belay_testkit::json;
//!
//! let fetch = job("etl", "fetch").payload(json!({"url": "https://example.com/a.csv"}));
//! let load = job("etl", "load").payload(json!({"from": placeholder("fetch", ".url")}));
//! let spec = workflow("import", [step("fetch", fetch), step("load", load).after("fetch")]);
//! assert_eq!(spec.steps.len(), 2);
//! assert_eq!(spec.steps[1].job.payload["from"], "{{steps.fetch.result.url}}");
//! assert_eq!(numbered(3), json!({"n": 3}));
//! ```

use std::time::Duration;

use belay_common::{BackoffPolicy, Timestamp};
use belay_json::{json, Value};
use belay_proto::workflow::StepSpec;
use belay_proto::{IntoQueueName, JobSpec, QueueName, ScheduleId, ScheduleSpec, WorkflowSpec};

use crate::error::{Error, Result};

/// Where manual clocks start by default: Monday 2025-03-03 08:00 UTC.
pub const START: &str = "2025-03-03T08:00:00Z";

/// The queue [`schedule`] templates enqueue into.
pub const SCHEDULED_QUEUE: &str = "scheduled";

/// Retries of [`job`] specs: 1 s doubling up to a minute, without jitter,
/// so a test on a manual clock knows exactly when the next attempt is due
/// (1 s after the first failure, 2 s after the second, ...).
pub const TEST_RETRY: BackoffPolicy =
    BackoffPolicy::exponential(Duration::from_secs(1), 2.0, Duration::from_secs(60));

/// [`START`] as a timestamp.
#[must_use]
pub fn start_time() -> Timestamp {
    // The constant is a valid RFC 3339 literal; the parser's own tests
    // cover this exact shape.
    Timestamp::parse_rfc3339(START).expect("START is a valid RFC 3339 timestamp")
}

/// A job spec with [`TEST_RETRY`] instead of the queue's jittered default
/// backoff. Everything else is [`JobSpec::new`]'s default.
#[must_use]
pub fn job(queue: impl IntoQueueName, kind: impl Into<String>) -> JobSpec {
    JobSpec::new(queue, kind).retry(TEST_RETRY)
}

/// `count` specs for `queue`, with payloads `{"n": 0}` to `{"n": count-1}`
/// so that each execution can be told apart.
#[must_use]
pub fn numbered_jobs(queue: &QueueName, kind: &str, count: usize) -> Vec<JobSpec> {
    (0..count)
        .map(|n| job(queue, kind).payload(numbered(n)))
        .collect()
}

/// The payload `{"n": n}`.
#[must_use]
pub fn numbered(n: usize) -> Value {
    json!({ "n": n })
}

/// The `n` of a [`numbered`] payload.
#[must_use]
pub fn number_of(payload: &Value) -> Option<u64> {
    payload["n"].as_u64()
}

/// A queue name built at run time, e.g. `format!("q{i}")`.
pub fn queue_name(name: impl Into<String>) -> Result<QueueName> {
    let name = name.into();
    QueueName::new(name.as_str()).map_err(|e| Error::Invalid(format!("queue {name:?}: {e}")))
}
