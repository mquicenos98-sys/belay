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

/// A UTC schedule `id` firing on `cron`, whose template is
/// `job("scheduled", id)`: the job kind is the schedule's id, which keeps
/// the jobs of several schedules apart. Chain the [`ScheduleSpec`] builder
/// methods for a zone or policies, or replace the template.
///
/// # Panics
///
/// If `id` is not a valid schedule id; like
/// [`ScheduleId::from_static`], this is meant for literals.
#[must_use]
pub fn schedule(id: &'static str, cron: &str) -> ScheduleSpec {
    let id = ScheduleId::from_static(id);
    let template = job(QueueName::from_static(SCHEDULED_QUEUE), id.as_str());
    ScheduleSpec::new(id, cron, template)
}

/// A workflow step running `job`; chain [`StepSpec::after`] for its
/// dependencies.
#[must_use]
pub fn step(name: &str, job: JobSpec) -> StepSpec {
    StepSpec::new(name, job)
}

/// A workflow made of `steps`, in order.
#[must_use]
pub fn workflow(name: &str, steps: impl IntoIterator<Item = StepSpec>) -> WorkflowSpec {
    steps
        .into_iter()
        .fold(WorkflowSpec::new(name), WorkflowSpec::step)
}

/// A payload placeholder for the result of step `step`: `path` is empty
/// for the whole result, `.a.b` for dotted keys or `/a/0` for a JSON
/// pointer. A string that is exactly one placeholder is replaced by the
/// value itself; inside a longer string it is spliced in as text.
#[must_use]
pub fn placeholder(step: &str, path: &str) -> String {
    format!("{{{{steps.{step}.result{path}}}}}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_common::Rng;
    use belay_proto::Validate;

    #[test]
    fn test_jobs_retry_on_a_predictable_curve() {
        let spec = job("emails", "send");
        let retry = spec.retry.expect("job() sets a retry policy");
        let mut rng = Rng::seed_from_u64(1);
        let delays: Vec<Duration> = (1..=7).map(|a| retry.delay_for(a, &mut rng)).collect();
        let secs: Vec<u64> = delays.iter().map(Duration::as_secs).collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 32, 60]);
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn numbered_jobs_carry_their_index() {
        let queue = queue_name("batch-7").unwrap();
        let specs = numbered_jobs(&queue, "work", 3);
        let ns: Vec<Option<u64>> = specs.iter().map(|s| number_of(&s.payload)).collect();
        assert_eq!(ns, [Some(0), Some(1), Some(2)]);
        assert!(specs.iter().all(|s| s.queue == queue && s.kind == "work"));
    }

    #[test]
    fn bad_runtime_queue_names_are_invalid_arguments() {
        let err = queue_name("Upper Case").unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.starts_with("queue \"Upper Case\"")));
    }

    #[test]
    fn schedules_name_their_jobs_after_themselves() {
        let spec = schedule("nightly-report", "0 3 * * *").timezone("America/Bogota");
        assert_eq!(spec.template.kind, "nightly-report");
        assert_eq!(spec.template.queue.as_str(), SCHEDULED_QUEUE);
        assert_eq!(spec.timezone, "America/Bogota");
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn workflows_keep_step_order_and_validate() {
        let spec = workflow(
            "diamond",
            [
                step("a", job("wf", "a")),
                step("b", job("wf", "b")).after("a"),
                step("c", job("wf", "c")).after("a"),
                step(
                    "d",
                    job("wf", "d").payload(json!({"x": placeholder("b", "/x")})),
                )
                .after("b")
                .after("c"),
            ],
        );
        let names: Vec<&str> = spec.steps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c", "d"]);
        assert!(spec.validate().is_ok(), "{:?}", spec.validate());
        assert_eq!(placeholder("b", ""), "{{steps.b.result}}");
    }

    #[test]
    fn the_default_start_is_a_monday_morning() {
        let civil = start_time().to_civil();
        assert_eq!(
            (civil.year, civil.month, civil.day, civil.hour),
            (2025, 3, 3, 8)
        );
    }
}
