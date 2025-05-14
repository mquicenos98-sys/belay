//! What a client submits: [`JobSpec`] and its parts.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use belay_common::text::humanize_bytes;
use belay_common::{format_duration, parse_duration, BackoffPolicy, Timestamp};
use belay_json::{FromJson, FromJsonError, ObjectBuilder, ObjectReader, ToJson, Value};

use crate::error::ParseError;
use crate::ids::{IntoQueueName, QueueName, TenantId, WorkflowId, LABEL_RULE, STEP_RULE};
use crate::job::JobState;
use crate::limits::*;
use crate::queue::QueueConfig;
use crate::validate::{Validate, Validator};
use crate::wire::{encoded_len, text_enum, text_json, BuilderExt, ReaderExt};

/// `meta` keys starting with this prefix are written by belay itself.
pub const RESERVED_META_PREFIX: &str = "belay.";
/// `meta` key naming the schedule that dispatched a job.
pub const META_SCHEDULE: &str = "belay.schedule";
/// `meta` key holding the nominal fire time (RFC 3339) of a scheduled job.
pub const META_FIRE_TIME: &str = "belay.fire_time";

/// A job as submitted: which handler to run, with what input, and how to
/// treat it.
///
/// Unset limits (`max_attempts`, `retry`, `timeout`) fall back to the
/// queue's configuration; the queue engine fills them in at enqueue time
/// with [`apply_queue_defaults`](Self::apply_queue_defaults), so a stored
/// job always carries the values it will run with even if the queue's
/// configuration changes later.
///
/// ```
/// use std::time::Duration;
/// use belay_json::{json, ToJson};
/// use belay_proto::{JobSpec, UniqueScope};
///
/// let spec = JobSpec::new("emails", "send_welcome")
///     .payload(json!({"user": 42}))
///     .priority(10)
///     .delay(Duration::from_secs(30))
///     .max_attempts(5)
///     .unique("user:42", UniqueScope::UntilFinished)
///     .tag("signup");
/// assert_eq!(
///     spec.to_json().to_string(),
///     concat!(
///         r#"{"queue":"emails","kind":"send_welcome","payload":{"user":42},"priority":10,"#,
///         r#""delay":"30s","max_attempts":5,"#,
///         r#""unique":{"key":"user:42","scope":"until_finished","on_conflict":"return_existing"},"#,
///         r#""tags":["signup"]}"#,
///     )
/// );
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct JobSpec {
    /// The queue to enqueue into.
    pub queue: QueueName,
    /// Handler name; workers dispatch on it.
    pub kind: String,
    /// Handler input. `null` when there is none.
    pub payload: Value,
    /// Higher runs first; `-1000..=1000`, default 0.
    pub priority: i16,
    /// Do not run before this time. Exclusive with `delay`.
    pub run_at: Option<Timestamp>,
    /// Do not run until this long after enqueue. Exclusive with `run_at`.
    pub delay: Option<Duration>,
    /// Total attempts allowed, including the first. `None`: queue default.
    pub max_attempts: Option<u32>,
    /// Delay curve between attempts. `None`: queue default.
    pub retry: Option<BackoffPolicy>,
    /// Execution budget of one attempt. `None`: queue default.
    pub timeout: Option<Duration>,
    /// Deduplication rule.
    pub unique: Option<Unique>,
    /// Free-form labels for filtering.
    pub tags: Vec<String>,
    /// Owner for fair sharing within the queue.
    pub tenant: Option<TenantId>,
    /// String annotations. Keys starting with `belay.` are set by belay.
    pub meta: BTreeMap<String, String>,
    /// Set on jobs that belong to a workflow step.
    pub workflow: Option<WorkflowRef>,
}

impl JobSpec {
    /// A job of `kind` on `queue` with every option at its default.
    ///
    /// `queue` may be a [`QueueName`] or a string literal; a literal that is
    /// not a valid queue name panics (see [`IntoQueueName`]). Parse names
    /// read at runtime into a `QueueName` first.
    #[must_use]
    pub fn new(queue: impl IntoQueueName, kind: impl Into<String>) -> JobSpec {
        JobSpec {
            queue: queue.into_queue_name(),
            kind: kind.into(),
            payload: Value::Null,
            priority: 0,
            run_at: None,
            delay: None,
            max_attempts: None,
            retry: None,
            timeout: None,
            unique: None,
            tags: Vec::new(),
            tenant: None,
            meta: BTreeMap::new(),
            workflow: None,
        }
    }

    /// Sets the payload.
    #[must_use]
    pub fn payload(mut self, payload: impl Into<Value>) -> JobSpec {
        self.payload = payload.into();
        self
    }

    /// Sets the payload from any [`ToJson`] value.
    #[must_use]
    pub fn payload_of<T: ToJson + ?Sized>(mut self, payload: &T) -> JobSpec {
        self.payload = payload.to_json();
        self
    }

    /// Sets the priority (higher runs first).
    #[must_use]
    pub fn priority(mut self, priority: i16) -> JobSpec {
        self.priority = priority;
        self
    }

    /// Runs no earlier than `at`. Clears any delay.
    #[must_use]
    pub fn run_at(mut self, at: Timestamp) -> JobSpec {
        self.run_at = Some(at);
        self.delay = None;
        self
    }

    /// Runs no earlier than `delay` after enqueue. Clears any `run_at`.
    #[must_use]
    pub fn delay(mut self, delay: Duration) -> JobSpec {
        self.delay = Some(delay);
        self.run_at = None;
        self
    }

    /// Allows at most `n` attempts in total.
    #[must_use]
    pub fn max_attempts(mut self, n: u32) -> JobSpec {
        self.max_attempts = Some(n);
        self
    }

    /// Uses `policy` between attempts instead of the queue's.
    #[must_use]
    pub fn retry(mut self, policy: BackoffPolicy) -> JobSpec {
        self.retry = Some(policy);
        self
    }

    /// Gives each attempt at most `timeout` to finish.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> JobSpec {
        self.timeout = Some(timeout);
        self
    }

    /// Deduplicates on `key` within `scope`; a conflicting enqueue returns
    /// the existing job (see [`Unique::on_conflict`] for other choices).
    #[must_use]
    pub fn unique(mut self, key: impl Into<String>, scope: UniqueScope) -> JobSpec {
        self.unique = Some(Unique::new(key, scope));
        self
    }

    /// Sets the full uniqueness rule.
    #[must_use]
    pub fn unique_with(mut self, unique: Unique) -> JobSpec {
        self.unique = Some(unique);
        self
    }

    /// Adds a tag.
    #[must_use]
    pub fn tag(mut self, tag: impl Into<String>) -> JobSpec {
        self.tags.push(tag.into());
        self
    }

    /// Sets the tenant.
    #[must_use]
    pub fn tenant(mut self, tenant: TenantId) -> JobSpec {
        self.tenant = Some(tenant);
        self
    }

    /// Adds a `meta` entry, replacing any previous value for `key`.
    #[must_use]
    pub fn meta(mut self, key: impl Into<String>, value: impl Into<String>) -> JobSpec {
        self.meta.insert(key.into(), value.into());
        self
    }

    /// Marks the job as a workflow step.
    #[must_use]
    pub fn workflow(mut self, workflow: WorkflowRef) -> JobSpec {
        self.workflow = Some(workflow);
        self
    }

    /// When a job enqueued at `now` first becomes eligible to run.
    #[must_use]
    pub fn run_time(&self, now: Timestamp) -> Timestamp {
        match (self.run_at, self.delay) {
            (Some(at), _) => at,
            (None, Some(delay)) => now + delay,
            (None, None) => now,
        }
    }

    /// Fills unset limits from the queue's configuration.
    pub fn apply_queue_defaults(&mut self, config: &QueueConfig) {
        self.max_attempts.get_or_insert(config.default_max_attempts);
        self.retry.get_or_insert(config.default_retry);
        self.timeout.get_or_insert(config.default_timeout);
    }

    /// Whether the job carries `tag`.
    #[must_use]
    pub fn has_tag(&self, tag: &str) -> bool {
        self.tags.iter().any(|t| t == tag)
    }
}

impl ToJson for JobSpec {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("queue", &self.queue)
            .field("kind", &self.kind)
            .field_if(!self.payload.is_null(), "payload", &self.payload)
            .field("priority", self.priority)
            .opt_timestamp("run_at", self.run_at)
            .opt_duration("delay", self.delay)
            .opt_field("max_attempts", &self.max_attempts)
            .opt_backoff("retry", self.retry.as_ref())
            .opt_duration("timeout", self.timeout)
            .opt_field("unique", &self.unique)
            .field_if(!self.tags.is_empty(), "tags", &self.tags)
            .opt_field("tenant", &self.tenant)
            .field_if(!self.meta.is_empty(), "meta", &self.meta)
            .opt_field("workflow", &self.workflow)
            .build()
    }
}

impl FromJson for JobSpec {
    fn from_json(value: &Value) -> Result<JobSpec, FromJsonError> {
        let r = ObjectReader::new(value, "JobSpec")?;
        let spec = JobSpec {
            queue: r.req("queue")?,
            kind: r.req("kind")?,
            payload: r.or_default("payload")?,
            priority: r.or("priority", 0)?,
            run_at: r.opt_timestamp("run_at")?,
            delay: r.opt_duration("delay")?,
            max_attempts: r.opt("max_attempts")?,
            retry: r.opt_backoff("retry")?,
            timeout: r.opt_duration("timeout")?,
            unique: r.opt("unique")?,
            tags: r.or_default("tags")?,
            tenant: r.opt("tenant")?,
            meta: r.or_default("meta")?,
            workflow: r.opt("workflow")?,
        };
        r.deny_unknown_fields()?;
        Ok(spec)
    }
}

impl Validate for JobSpec {
    fn validate_into(&self, v: &mut Validator) {
        v.check_name("kind", &self.kind, LABEL_RULE.max(MAX_KIND_LEN));
        let size = encoded_len(&self.payload);
        v.check(size <= MAX_PAYLOAD_BYTES, "payload", || {
            format!(
                "is {} as JSON; the limit is {}",
                humanize_bytes(size as u64),
                humanize_bytes(MAX_PAYLOAD_BYTES as u64)
            )
        });
        v.check_range("priority", self.priority, MIN_PRIORITY, MAX_PRIORITY);
        if let Some(delay) = self.delay {
            v.check(self.run_at.is_none(), "delay", || {
                "cannot be combined with run_at; set one or the other".to_owned()
            });
            v.check_duration("delay", delay, Duration::ZERO, MAX_DELAY);
        }
        if let Some(n) = self.max_attempts {
            v.check_range("max_attempts", n, 1, MAX_ATTEMPTS);
        }
        if let Some(timeout) = self.timeout {
            v.check_duration("timeout", timeout, MIN_TIMEOUT, MAX_TIMEOUT);
        }
        if let Some(unique) = &self.unique {
            v.nested("unique", unique);
        }
        validate_tags(v, &self.tags);
        validate_meta(v, &self.meta);
        if let Some(wf) = &self.workflow {
            v.at_key("workflow", |v| v.check_name("step", &wf.step, STEP_RULE));
        }
    }
}

fn validate_tags(v: &mut Validator, tags: &[String]) {
    v.check(tags.len() <= MAX_TAGS, "tags", || {
        format!("at most {MAX_TAGS} tags are allowed, got {}", tags.len())
    });
    for (i, tag) in tags.iter().enumerate() {
        if let Some(problem) = LABEL_RULE.max(MAX_TAG_LEN).problem(tag) {
            v.at_index("tags", i, |v| v.report_here(problem));
        } else if tags[..i].contains(tag) {
            v.at_index("tags", i, |v| {
                v.report_here(format!("duplicate tag {}", crate::error::quote(tag)))
            });
        }
    }
}

/// Shared with workflows, whose `meta` follows the same rules.
pub(crate) fn validate_meta(v: &mut Validator, meta: &BTreeMap<String, String>) {
    v.check(meta.len() <= MAX_META_ENTRIES, "meta", || {
        format!(
            "at most {MAX_META_ENTRIES} entries are allowed, got {}",
            meta.len()
        )
    });
    v.at_key("meta", |v| {
        for (key, value) in meta {
            if let Some(problem) = LABEL_RULE.max(MAX_META_KEY_LEN).problem(key) {
                v.report(key, format!("invalid key: {problem}"));
            }
            v.check_bytes(key, value, MAX_META_VALUE_BYTES);
        }
    });
}

/// How long a uniqueness key stays claimed by the job that holds it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UniqueScope {
    /// Until the job first starts running. Text form `until_started`.
    UntilStarted,
    /// Until the job reaches a terminal state. Text form `until_finished`;
    /// the default.
    #[default]
    UntilFinished,
    /// For a fixed period after enqueue, whatever happens to the job. Text
    /// form `for:<duration>`, e.g. `for:1h`.
    For(Duration),
}

impl UniqueScope {
    /// Whether a job in `state`, created at `created_at`, still holds its key
    /// at `now`.
    #[must_use]
    pub fn holds(&self, state: JobState, created_at: Timestamp, now: Timestamp) -> bool {
        match *self {
            UniqueScope::UntilStarted => matches!(state, JobState::Scheduled | JobState::Ready),
            UniqueScope::UntilFinished => !state.is_terminal(),
            UniqueScope::For(period) => now < created_at + period,
        }
    }
}

impl fmt::Display for UniqueScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UniqueScope::UntilStarted => f.write_str("until_started"),
            UniqueScope::UntilFinished => f.write_str("until_finished"),
            UniqueScope::For(period) => write!(f, "for:{}", format_duration(*period)),
        }
    }
}

impl FromStr for UniqueScope {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<UniqueScope, ParseError> {
        const NAMES: &[&str] = &["until_started", "until_finished", "for:<duration>"];
        match s {
            "until_started" => Ok(UniqueScope::UntilStarted),
            "until_finished" => Ok(UniqueScope::UntilFinished),
            _ => match s.strip_prefix("for:") {
                Some(period) => parse_duration(period)
                    .map(UniqueScope::For)
                    .map_err(|e| ParseError::invalid("unique scope", s, e)),
                None => Err(ParseError::unknown("unique scope", s, NAMES)),
            },
        }
    }
}

text_json!(UniqueScope, "unique scope");

text_enum! {
    /// What an enqueue does when its uniqueness key is already held.
    #[derive(Default)]
    OnConflict, "conflict policy" {
        /// Fail with `already_exists`.
        Reject = "reject",
        /// Succeed without enqueueing, returning the holder's id with status
        /// `duplicate`. The default, because it makes enqueue idempotent.
        #[default]
        ReturnExisting = "return_existing",
        /// Cancel the holder if it has not started yet and enqueue the new
        /// job (status `replaced`); behaves like `return_existing` if the
        /// holder is already running.
        Replace = "replace",
    }
}

/// A deduplication rule: at most one job per queue holds `key` at a time.
///
/// In JSON it is an object, or just the key as a string when the defaults
/// (`until_finished`, `return_existing`) are wanted:
/// `"unique": "user:42"`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unique {
    /// The deduplication key, e.g. `user:42`.
    pub key: String,
    /// How long the key stays claimed.
    pub scope: UniqueScope,
    /// What a conflicting enqueue does.
    pub on_conflict: OnConflict,
}

impl Unique {
    /// A rule for `key` within `scope` that returns the existing job on
    /// conflict.
    #[must_use]
    pub fn new(key: impl Into<String>, scope: UniqueScope) -> Unique {
        Unique {
            key: key.into(),
            scope,
            on_conflict: OnConflict::default(),
        }
    }

    /// Sets the conflict behaviour.
    #[must_use]
    pub fn on_conflict(mut self, on_conflict: OnConflict) -> Unique {
        self.on_conflict = on_conflict;
        self
    }
}

impl ToJson for Unique {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("key", &self.key)
            .field("scope", self.scope)
            .field("on_conflict", self.on_conflict)
            .build()
    }
}

impl FromJson for Unique {
    fn from_json(value: &Value) -> Result<Unique, FromJsonError> {
        if let Some(key) = value.as_str() {
            return Ok(Unique::new(key, UniqueScope::default()));
        }
        let r = ObjectReader::new(value, "Unique")?;
        let unique = Unique {
            key: r.req("key")?,
            scope: r.or_default("scope")?,
            on_conflict: r.or_default("on_conflict")?,
        };
        r.deny_unknown_fields()?;
        Ok(unique)
    }
}

impl Validate for Unique {
    fn validate_into(&self, v: &mut Validator) {
        if self.key.is_empty() {
            v.report("key", "must not be empty");
        } else if self.key.chars().any(char::is_control) {
            v.report("key", "must not contain control characters");
        }
        v.check_bytes("key", &self.key, MAX_UNIQUE_KEY_BYTES);
        if let UniqueScope::For(period) = self.scope {
            v.check_duration("scope", period, Duration::from_secs(1), MAX_UNIQUE_FOR);
        }
    }
}

/// Links a job to the workflow step it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowRef {
    /// The workflow run.
    pub id: WorkflowId,
    /// The step name within it.
    pub step: String,
}

impl ToJson for WorkflowRef {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("id", self.id)
            .field("step", &self.step)
            .build()
    }
}

impl FromJson for WorkflowRef {
    fn from_json(value: &Value) -> Result<WorkflowRef, FromJsonError> {
        let r = ObjectReader::new(value, "WorkflowRef")?;
        let wf = WorkflowRef {
            id: r.req("id")?,
            step: r.req("step")?,
        };
        r.deny_unknown_fields()?;
        Ok(wf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_json::json;

    fn problems(spec: &JobSpec) -> Vec<String> {
        match spec.validate() {
            Ok(()) => Vec::new(),
            Err(e) => e.iter().map(ToString::to_string).collect(),
        }
    }

    fn full_spec() -> JobSpec {
        JobSpec::new("billing:invoices", "render_pdf")
            .payload(json!({"invoice": 991, "lines": [1, 2]}))
            .priority(-5)
            .run_at("2025-06-01T08:00:00Z".parse().unwrap())
            .max_attempts(3)
            .retry("exp:1s*2..10m~full".parse().unwrap())
            .timeout(Duration::from_secs(300))
            .unique_with(
                Unique::new("invoice:991", UniqueScope::For(Duration::from_secs(3600)))
                    .on_conflict(OnConflict::Replace),
            )
            .tag("pdf")
            .tag("eu")
            .tenant("acme".parse().unwrap())
            .meta("requested_by", "ops@example.com")
            .workflow(WorkflowRef {
                id: "01HQ3K9Z8N6V2T4R5S7W0X1Y2Z".parse().unwrap(),
                step: "render".into(),
            })
    }

    #[test]
    fn full_spec_round_trips_through_json_text() {
        let spec = full_spec();
        let text = belay_json::to_string_of(&spec);
        assert_eq!(
            text,
            concat!(
                r#"{"queue":"billing:invoices","kind":"render_pdf","payload":{"invoice":991,"lines":[1,2]},"#,
                r#""priority":-5,"run_at":"2025-06-01T08:00:00.000Z","max_attempts":3,"#,
                r#""retry":"exp:1s*2..10m~full","timeout":"5m","#,
                r#""unique":{"key":"invoice:991","scope":"for:1h","on_conflict":"replace"},"#,
                r#""tags":["pdf","eu"],"tenant":"acme","meta":{"requested_by":"ops@example.com"},"#,
                r#""workflow":{"id":"01HQ3K9Z8N6V2T4R5S7W0X1Y2Z","step":"render"}}"#
            )
        );
        assert_eq!(belay_json::from_str::<JobSpec>(&text).unwrap(), spec);
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn minimal_spec_uses_defaults_and_nulls_count_as_absent() {
        let v = json!({"queue": "q", "kind": "k", "payload": null, "tags": null, "retry": null});
        let spec = JobSpec::from_json(&v).unwrap();
        assert_eq!(spec, JobSpec::new("q", "k"));
        assert_eq!(
            spec.to_json(),
            json!({"queue": "q", "kind": "k", "priority": 0})
        );
    }

    #[test]
    fn decode_errors_point_at_the_field() {
        let cases = [
            (
                json!({"kind": "k"}),
                r#"$: missing required field "queue" in JobSpec"#,
            ),
            (
                json!({"queue": "Q", "kind": "k"}),
                r#"$.queue: invalid queue name "Q": must start with a lowercase letter or digit, got 'Q'"#,
            ),
            (
                json!({"queue": "q", "kind": "k", "priority": 40000}),
                "$.priority: expected integer in range -32768..=32767, found 40000",
            ),
            (
                json!({"queue": "q", "kind": "k", "delay": 30}),
                r#"$.delay: expected duration string such as "30s" or "1h30m", found 30"#,
            ),
            (
                json!({"queue": "q", "kind": "k", "unique": {"key": "a", "scope": "until_finish"}}),
                r#"$.unique.scope: unknown unique scope "until_finish", did you mean "until_finished"?"#,
            ),
            (
                json!({"queue": "q", "kind": "k", "unique": {"key": "a", "scope": "for:1y"}}),
                r#"$.unique.scope: invalid unique scope "for:1y": invalid duration "1y": unknown unit "y" (expected ns, us, ms, s, m, h, d or w)"#,
            ),
            (
                json!({"queue": "q", "kind": "k", "workflow": {"id": "x", "step": "s"}}),
                r#"$.workflow.id: invalid workflow id "x": expected 26 characters, got 1"#,
            ),
            (
                json!({"queue": "q", "kind": "k", "meta": {"n": 1}}),
                "$.meta.n: expected string, found 1",
            ),
            (
                json!({"queue": "q", "kind": "k", "max_attempt": 3}),
                r#"$: unknown field "max_attempt" in JobSpec, did you mean "max_attempts"?"#,
            ),
        ];
        for (input, expected) in cases {
            let err = JobSpec::from_json(&input).unwrap_err();
            assert_eq!(err.to_string(), expected, "input {input}");
        }
    }

    #[test]
    fn unique_accepts_a_bare_key() {
        let v = json!({"queue": "q", "kind": "k", "unique": "user:1"});
        let spec = JobSpec::from_json(&v).unwrap();
        assert_eq!(
            spec.unique,
            Some(Unique::new("user:1", UniqueScope::UntilFinished))
        );
        assert_eq!(
            spec.to_json()["unique"],
            json!({"key": "user:1", "scope": "until_finished", "on_conflict": "return_existing"})
        );
    }

    #[test]
    fn validation_reports_every_problem_with_its_path() {
        let mut spec = JobSpec::new("q", "send email")
            .priority(5000)
            .max_attempts(0)
            .timeout(Duration::from_millis(10))
            .unique("", UniqueScope::For(Duration::from_secs(90 * 86_400)))
            .tag("ok")
            .tag("ok")
            .tag("-bad")
            .meta("bad key", "v")
            .meta("note", "x".repeat(2000));
        spec.run_at = Some(Timestamp::from_millis(0));
        spec.delay = Some(Duration::from_secs(1));
        assert_eq!(
            problems(&spec),
            [
                "$.kind: ' ' at position 4 is not allowed (use letters, digits, '_', '.', ':', '/' or '-')",
                "$.priority: must be between -1000 and 1000, got 5000",
                "$.delay: cannot be combined with run_at; set one or the other",
                "$.max_attempts: must be between 1 and 1000, got 0",
                "$.timeout: must be between 1s and 7d, got 10ms",
                "$.unique.key: must not be empty",
                "$.unique.scope: must be between 1s and 30d, got 90d",
                r#"$.tags[1]: duplicate tag "ok""#,
                "$.tags[2]: must start with a letter or digit, got '-'",
                r#"$.meta["bad key"]: invalid key: ' ' at position 3 is not allowed (use letters, digits, '_', '.', ':', '/' or '-')"#,
                "$.meta.note: must be at most 1024 bytes, got 2000",
            ]
        );
    }

    #[test]
    fn payload_limit_counts_serialized_bytes() {
        let big = "x".repeat(MAX_PAYLOAD_BYTES);
        let spec = JobSpec::new("q", "k").payload(json!({ "blob": big }));
        assert_eq!(
            problems(&spec),
            ["$.payload: is 1.0 MiB as JSON; the limit is 1.0 MiB"]
        );
        // A string payload serializes with two quotes around it.
        let fits = JobSpec::new("q", "k").payload("x".repeat(MAX_PAYLOAD_BYTES - 2));
        assert!(fits.validate().is_ok());
    }

    #[test]
    fn too_many_tags_and_meta_entries() {
        let mut spec = JobSpec::new("q", "k");
        spec.tags = (0..17).map(|i| format!("t{i}")).collect();
        spec.meta = (0..33).map(|i| (format!("k{i}"), String::new())).collect();
        assert_eq!(
            problems(&spec),
            [
                "$.tags: at most 16 tags are allowed, got 17",
                "$.meta: at most 32 entries are allowed, got 33"
            ]
        );
    }

    #[test]
    fn run_time_honours_run_at_then_delay() {
        let now = Timestamp::from_millis(1_000);
        assert_eq!(JobSpec::new("q", "k").run_time(now), now);
        let delayed = JobSpec::new("q", "k").delay(Duration::from_secs(2));
        assert_eq!(delayed.run_time(now), Timestamp::from_millis(3_000));
        // The builder keeps the two exclusive: the later call wins.
        let at = delayed.run_at(Timestamp::from_millis(50));
        assert_eq!(
            (at.delay, at.run_time(now)),
            (None, Timestamp::from_millis(50))
        );
    }

    #[test]
    fn queue_defaults_fill_only_unset_limits() {
        let config = QueueConfig {
            default_max_attempts: 7,
            default_timeout: Duration::from_secs(60),
            ..QueueConfig::default()
        };
        let mut spec = JobSpec::new("q", "k").max_attempts(2);
        spec.apply_queue_defaults(&config);
        assert_eq!(spec.max_attempts, Some(2));
        assert_eq!(spec.timeout, Some(Duration::from_secs(60)));
        assert_eq!(spec.retry, Some(config.default_retry));
    }

    #[test]
    fn unique_scopes_release_at_the_documented_moment() {
        let created = Timestamp::from_millis(0);
        let now = Timestamp::from_millis(10_000);
        let started = UniqueScope::UntilStarted;
        assert!(started.holds(JobState::Scheduled, created, now));
        assert!(started.holds(JobState::Ready, created, now));
        assert!(!started.holds(JobState::Running, created, now));
        assert!(!started.holds(JobState::Retrying, created, now));
        let finished = UniqueScope::UntilFinished;
        assert!(finished.holds(JobState::Retrying, created, now));
        assert!(!finished.holds(JobState::Dead, created, now));
        let timed = UniqueScope::For(Duration::from_secs(10));
        assert!(timed.holds(JobState::Succeeded, created, Timestamp::from_millis(9_999)));
        assert!(!timed.holds(JobState::Ready, created, now));
    }

    #[test]
    fn text_forms_round_trip() {
        for s in ["until_started", "until_finished", "for:1h30m"] {
            assert_eq!(s.parse::<UniqueScope>().unwrap().to_string(), s);
        }
        for s in ["reject", "return_existing", "replace"] {
            assert_eq!(s.parse::<OnConflict>().unwrap().to_string(), s);
        }
        assert_eq!(
            "ignore".parse::<OnConflict>().unwrap_err().to_string(),
            r#"unknown conflict policy "ignore", expected one of: reject, return_existing, replace"#
        );
    }
}
