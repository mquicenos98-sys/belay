//! Bodies for enqueueing (`POST /v1/jobs`) and for the worker protocol:
//! `POST /v1/lease`, then `POST /v1/jobs/:id/heartbeat | complete | fail`.

use std::time::Duration;

use belay_common::Timestamp;
use belay_json::{FromJson, FromJsonError, ObjectBuilder, ObjectReader, ToJson, Value};

use crate::ids::{JobId, LeaseToken, QueueName, WorkerId};
use crate::job::{validate_error_kind, Job, JobError, JobSpec, JobState, Progress};
use crate::limits::{
    DEFAULT_LEASE, MAX_DELAY, MAX_ENQUEUE_BATCH, MAX_LEASE, MAX_LEASE_JOBS, MAX_LEASE_QUEUES,
    MAX_LEASE_WAIT, MAX_RESULT_BYTES, MIN_LEASE,
};
use crate::validate::{Validate, Validator};
use crate::wire::{encoded_len, text_enum, BuilderExt, ReaderExt};

/// `POST /v1/jobs` body: a bare [`JobSpec`] or `{"jobs": [...]}`.
///
/// ```
/// use belay_proto::api::EnqueueRequest;
///
/// let one: EnqueueRequest = belay_json::from_str(r#"{"queue": "emails", "kind": "send"}"#).unwrap();
/// let many: EnqueueRequest = belay_json::from_str(
///     r#"{"jobs": [{"queue": "emails", "kind": "send"}, {"queue": "sms", "kind": "send"}]}"#,
/// ).unwrap();
/// assert_eq!((one.jobs().len(), many.jobs().len()), (1, 2));
/// ```
#[derive(Clone, Debug, PartialEq)]
pub enum EnqueueRequest {
    /// A single spec, sent as the body itself.
    One(JobSpec),
    /// Several specs, enqueued in order.
    Batch(Vec<JobSpec>),
}

impl EnqueueRequest {
    /// The specs, in order.
    #[must_use]
    pub fn jobs(&self) -> &[JobSpec] {
        match self {
            EnqueueRequest::One(spec) => std::slice::from_ref(spec),
            EnqueueRequest::Batch(specs) => specs,
        }
    }

    /// The specs, owned.
    #[must_use]
    pub fn into_jobs(self) -> Vec<JobSpec> {
        match self {
            EnqueueRequest::One(spec) => vec![spec],
            EnqueueRequest::Batch(specs) => specs,
        }
    }
}

impl From<JobSpec> for EnqueueRequest {
    fn from(spec: JobSpec) -> EnqueueRequest {
        EnqueueRequest::One(spec)
    }
}

impl From<Vec<JobSpec>> for EnqueueRequest {
    fn from(specs: Vec<JobSpec>) -> EnqueueRequest {
        EnqueueRequest::Batch(specs)
    }
}

impl ToJson for EnqueueRequest {
    fn to_json(&self) -> Value {
        match self {
            EnqueueRequest::One(spec) => spec.to_json(),
            EnqueueRequest::Batch(specs) => ObjectBuilder::new().field("jobs", specs).build(),
        }
    }
}

impl FromJson for EnqueueRequest {
    fn from_json(value: &Value) -> Result<EnqueueRequest, FromJsonError> {
        // A JobSpec never has a `jobs` field, so its presence decides.
        if value.get("jobs").is_none() {
            return JobSpec::from_json(value).map(EnqueueRequest::One);
        }
        let r = ObjectReader::new(value, "EnqueueRequest")?;
        let jobs = r.req("jobs")?;
        r.deny_unknown_fields()?;
        Ok(EnqueueRequest::Batch(jobs))
    }
}

impl Validate for EnqueueRequest {
    fn validate_into(&self, v: &mut Validator) {
        match self {
            EnqueueRequest::One(spec) => spec.validate_into(v),
            EnqueueRequest::Batch(specs) => {
                if specs.is_empty() {
                    v.report("jobs", "must contain at least one job");
                } else if specs.len() > MAX_ENQUEUE_BATCH {
                    v.report(
                        "jobs",
                        format!(
                            "at most {MAX_ENQUEUE_BATCH} jobs per request, got {}",
                            specs.len()
                        ),
                    );
                } else {
                    v.nested("jobs", specs);
                }
            }
        }
    }
}

text_enum! {
    /// What an enqueue did with one spec.
    EnqueueStatus, "enqueue status" {
        /// A new job was created.
        Created = "created",
        /// The uniqueness key was held; `id` is the existing job.
        Duplicate = "duplicate",
        /// The uniqueness key's holder was cancelled and a new job created.
        Replaced = "replaced",
    }
}

/// The outcome for one spec of an enqueue request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnqueueResult {
    /// The new job, or the existing one for `duplicate`.
    pub id: JobId,
    /// What happened.
    pub status: EnqueueStatus,
    /// The job's state after the call (`scheduled` or `ready` for new jobs).
    pub state: JobState,
}

impl ToJson for EnqueueResult {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("id", self.id)
            .field("status", self.status)
            .field("state", self.state)
            .build()
    }
}

impl FromJson for EnqueueResult {
    fn from_json(value: &Value) -> Result<EnqueueResult, FromJsonError> {
        let r = ObjectReader::new(value, "EnqueueResult")?;
        Ok(EnqueueResult {
            id: r.req("id")?,
            status: r.req("status")?,
            state: r.req("state")?,
        })
    }
}

/// `POST /v1/jobs` response: one result per submitted spec, in order,
/// whichever body form was used.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnqueueResponse {
    /// Per-spec outcomes.
    pub results: Vec<EnqueueResult>,
}

impl ToJson for EnqueueResponse {
    fn to_json(&self) -> Value {
        ObjectBuilder::new().field("results", &self.results).build()
    }
}

impl FromJson for EnqueueResponse {
    fn from_json(value: &Value) -> Result<EnqueueResponse, FromJsonError> {
        let r = ObjectReader::new(value, "EnqueueResponse")?;
        Ok(EnqueueResponse {
            results: r.req("results")?,
        })
    }
}

/// `POST /v1/lease` body: hand me up to `max_jobs` jobs from `queues`.
///
/// Queues are tried in the engine's queue order: round robin by default,
/// strictly as listed with `engine.queue_order = "in_order"`. With `wait`,
/// the server holds the request open until a job
/// arrives or the wait runs out (long polling), then answers with whatever
/// it has, possibly nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseRequest {
    /// Who is asking.
    pub worker: WorkerId,
    /// Where to look; see the type docs for the order.
    pub queues: Vec<QueueName>,
    /// At most this many jobs; default 1.
    pub max_jobs: u32,
    /// Lease length; heartbeats extend it. Default 30s.
    pub lease_for: Duration,
    /// Long-poll for up to this long when nothing is ready.
    pub wait: Option<Duration>,
}

impl LeaseRequest {
    /// One job at a time with the default lease, no waiting.
    #[must_use]
    pub fn new(worker: WorkerId, queues: Vec<QueueName>) -> LeaseRequest {
        LeaseRequest {
            worker,
            queues,
            max_jobs: 1,
            lease_for: DEFAULT_LEASE,
            wait: None,
        }
    }

    /// Sets the batch size.
    #[must_use]
    pub fn max_jobs(mut self, n: u32) -> LeaseRequest {
        self.max_jobs = n;
        self
    }

    /// Sets the lease length.
    #[must_use]
    pub fn lease_for(mut self, d: Duration) -> LeaseRequest {
        self.lease_for = d;
        self
    }

    /// Enables long polling.
    #[must_use]
    pub fn wait(mut self, d: Duration) -> LeaseRequest {
        self.wait = Some(d);
        self
    }
}

impl ToJson for LeaseRequest {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("worker", &self.worker)
            .field("queues", &self.queues)
            .field("max_jobs", self.max_jobs)
            .duration("lease_for", self.lease_for)
            .opt_duration("wait", self.wait)
            .build()
    }
}

impl FromJson for LeaseRequest {
    fn from_json(value: &Value) -> Result<LeaseRequest, FromJsonError> {
        let r = ObjectReader::new(value, "LeaseRequest")?;
        let req = LeaseRequest {
            worker: r.req("worker")?,
            queues: r.req("queues")?,
            max_jobs: r.or("max_jobs", 1)?,
            lease_for: r.duration_or("lease_for", DEFAULT_LEASE)?,
            wait: r.opt_duration("wait")?,
        };
        r.deny_unknown_fields()?;
        Ok(req)
    }
}

impl Validate for LeaseRequest {
    fn validate_into(&self, v: &mut Validator) {
        if self.queues.is_empty() {
            v.report("queues", "must name at least one queue");
        } else if self.queues.len() > MAX_LEASE_QUEUES {
            v.report(
                "queues",
                format!(
                    "at most {MAX_LEASE_QUEUES} queues, got {}",
                    self.queues.len()
                ),
            );
        }
        for (i, q) in self.queues.iter().enumerate() {
            if self.queues[..i].contains(q) {
                v.at_index("queues", i, |v| {
                    v.report_here(format!("duplicate queue {q}"))
                });
            }
        }
        v.check_range("max_jobs", self.max_jobs, 1, MAX_LEASE_JOBS);
        v.check_duration("lease_for", self.lease_for, MIN_LEASE, MAX_LEASE);
        if let Some(wait) = self.wait {
            v.check_duration("wait", wait, Duration::ZERO, MAX_LEASE_WAIT);
        }
    }
}

/// A job handed to a worker, with the token for later calls.
#[derive(Clone, Debug, PartialEq)]
pub struct LeasedJob {
    /// The job, in state `running`.
    pub job: Job,
    /// Present this in heartbeat, complete and fail calls.
    pub token: LeaseToken,
}

impl ToJson for LeasedJob {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("job", &self.job)
            .field("token", self.token)
            .build()
    }
}

impl FromJson for LeasedJob {
    fn from_json(value: &Value) -> Result<LeasedJob, FromJsonError> {
        let r = ObjectReader::new(value, "LeasedJob")?;
        Ok(LeasedJob {
            job: r.req("job")?,
            token: r.req("token")?,
        })
    }
}

/// `POST /v1/lease` response; empty when nothing was ready in time.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LeaseResponse {
    /// The leased jobs.
    pub jobs: Vec<LeasedJob>,
}

impl ToJson for LeaseResponse {
    fn to_json(&self) -> Value {
        ObjectBuilder::new().field("jobs", &self.jobs).build()
    }
}

impl FromJson for LeaseResponse {
    fn from_json(value: &Value) -> Result<LeaseResponse, FromJsonError> {
        let r = ObjectReader::new(value, "LeaseResponse")?;
        Ok(LeaseResponse {
            jobs: r.or_default("jobs")?,
        })
    }
}

/// `POST /v1/jobs/:id/heartbeat` body: I am still working on it.
#[derive(Clone, Debug, PartialEq)]
pub struct HeartbeatRequest {
    /// The lease token.
    pub token: LeaseToken,
    /// New lease length from now; `None` renews for the original lease
    /// length. A heartbeat never shortens a lease.
    pub extend_by: Option<Duration>,
    /// Progress to record.
    pub progress: Option<Progress>,
}

impl HeartbeatRequest {
    /// A plain renewal.
    #[must_use]
    pub fn new(token: LeaseToken) -> HeartbeatRequest {
        HeartbeatRequest {
            token,
            extend_by: None,
            progress: None,
        }
    }
}

impl ToJson for HeartbeatRequest {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("token", self.token)
            .opt_duration("extend_by", self.extend_by)
            .opt_field("progress", &self.progress)
            .build()
    }
}

impl FromJson for HeartbeatRequest {
    fn from_json(value: &Value) -> Result<HeartbeatRequest, FromJsonError> {
        let r = ObjectReader::new(value, "HeartbeatRequest")?;
        let req = HeartbeatRequest {
            token: r.req("token")?,
            extend_by: r.opt_duration("extend_by")?,
            progress: r.opt("progress")?,
        };
        r.deny_unknown_fields()?;
        Ok(req)
    }
}

impl Validate for HeartbeatRequest {
    fn validate_into(&self, v: &mut Validator) {
        if let Some(d) = self.extend_by {
            v.check_duration("extend_by", d, MIN_LEASE, MAX_LEASE);
        }
        if let Some(p) = &self.progress {
            v.nested("progress", p);
        }
    }
}

/// `POST /v1/jobs/:id/heartbeat` response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeartbeatResponse {
    /// When the lease now expires.
    pub expires_at: Timestamp,
    /// Cancellation was requested: stop, then report with `fail`.
    pub cancel_requested: bool,
}

impl ToJson for HeartbeatResponse {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .timestamp("expires_at", self.expires_at)
            .field("cancel_requested", self.cancel_requested)
            .build()
    }
}

impl FromJson for HeartbeatResponse {
    fn from_json(value: &Value) -> Result<HeartbeatResponse, FromJsonError> {
        let r = ObjectReader::new(value, "HeartbeatResponse")?;
        Ok(HeartbeatResponse {
            expires_at: r.req_timestamp("expires_at")?,
            cancel_requested: r.or_default("cancel_requested")?,
        })
    }
}

/// `POST /v1/jobs/:id/complete` body: it worked.
#[derive(Clone, Debug, PartialEq)]
pub struct CompleteRequest {
    /// The lease token.
    pub token: LeaseToken,
    /// What the handler returned. `Some(Value::Null)` (an explicit `null`)
    /// is kept apart from `None` (no `result` field).
    pub result: Option<Value>,
}

impl CompleteRequest {
    /// A completion without a result.
    #[must_use]
    pub fn new(token: LeaseToken) -> CompleteRequest {
        CompleteRequest {
            token,
            result: None,
        }
    }
}

impl ToJson for CompleteRequest {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("token", self.token)
            .opt_field("result", &self.result)
            .build()
    }
}

impl FromJson for CompleteRequest {
    fn from_json(value: &Value) -> Result<CompleteRequest, FromJsonError> {
        let r = ObjectReader::new(value, "CompleteRequest")?;
        let req = CompleteRequest {
            token: r.req("token")?,
            result: r.value("result").cloned(),
        };
        r.deny_unknown_fields()?;
        Ok(req)
    }
}

impl Validate for CompleteRequest {
    fn validate_into(&self, v: &mut Validator) {
        if let Some(result) = &self.result {
            let size = encoded_len(result);
            v.check(size <= MAX_RESULT_BYTES, "result", || {
                format!("is {size} bytes as JSON; the limit is {MAX_RESULT_BYTES}")
            });
        }
    }
}

/// `POST /v1/jobs/:id/fail` body: this attempt failed.
///
/// The message is never rejected for its length (a worker must always be
/// able to report a failure); it is truncated when recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailRequest {
    /// The lease token.
    pub token: LeaseToken,
    /// What went wrong.
    pub error: String,
    /// Machine-readable class, e.g. `timeout`.
    pub kind: Option<String>,
    /// Whether another attempt may help; default true.
    pub retryable: bool,
    /// Retry after exactly this long instead of the backoff policy's delay
    /// (e.g. from a `Retry-After` header).
    pub retry_in: Option<Duration>,
}

impl FailRequest {
    /// A retryable failure.
    #[must_use]
    pub fn new(token: LeaseToken, error: impl Into<String>) -> FailRequest {
        FailRequest {
            token,
            error: error.into(),
            kind: None,
            retryable: true,
            retry_in: None,
        }
    }

    /// The error-history entry this failure records.
    #[must_use]
    pub fn to_job_error(&self, at: Timestamp, attempt: u32, worker: Option<WorkerId>) -> JobError {
        JobError {
            kind: self.kind.clone(),
            worker,
            ..JobError::new(at, attempt, self.error.as_str()).retryable(self.retryable)
        }
    }
}

impl ToJson for FailRequest {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("token", self.token)
            .field("error", &self.error)
            .opt_field("kind", &self.kind)
            .field("retryable", self.retryable)
            .opt_duration("retry_in", self.retry_in)
            .build()
    }
}

impl FromJson for FailRequest {
    fn from_json(value: &Value) -> Result<FailRequest, FromJsonError> {
        let r = ObjectReader::new(value, "FailRequest")?;
        let req = FailRequest {
            token: r.req("token")?,
            error: r.req("error")?,
            kind: r.opt("kind")?,
            retryable: r.or("retryable", true)?,
            retry_in: r.opt_duration("retry_in")?,
        };
        r.deny_unknown_fields()?;
        Ok(req)
    }
}

impl Validate for FailRequest {
    fn validate_into(&self, v: &mut Validator) {
        if let Some(kind) = &self.kind {
            validate_error_kind(v, kind);
        }
        if let Some(d) = self.retry_in {
            v.check_duration("retry_in", d, Duration::ZERO, MAX_DELAY);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::MAX_ERROR_MESSAGE_BYTES;
    use belay_json::json;

    fn token() -> LeaseToken {
        LeaseToken::new(0xa1)
    }

    fn lines(r: Result<(), crate::Violations>) -> Vec<String> {
        match r {
            Ok(()) => Vec::new(),
            Err(e) => e.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn enqueue_request_keeps_its_wire_form() {
        let one = EnqueueRequest::from(JobSpec::new("q", "k"));
        assert_eq!(
            one.to_json(),
            json!({"queue": "q", "kind": "k", "priority": 0})
        );
        let batch = EnqueueRequest::from(vec![JobSpec::new("q", "a"), JobSpec::new("q", "b")]);
        assert_eq!(batch.to_json()["jobs"][1]["kind"], "b");
        for req in [one, batch] {
            assert_eq!(EnqueueRequest::from_json(&req.to_json()), Ok(req));
        }
    }

    #[test]
    fn enqueue_paths_follow_the_body_shape() {
        let err = EnqueueRequest::from_json(&json!({"queue": "q", "kind": "k", "priorty": 1}))
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            r#"$: unknown field "priorty" in JobSpec, did you mean "priority"?"#
        );
        let err = EnqueueRequest::from_json(
            &json!({"jobs": [{"queue": "q", "kind": "k"}, {"queue": "q"}]}),
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            r#"$.jobs[1]: missing required field "kind" in JobSpec"#
        );
        let err = EnqueueRequest::from_json(&json!({"jobs": [], "queue": "q"})).unwrap_err();
        assert_eq!(
            err.to_string(),
            r#"$: unknown field "queue" in EnqueueRequest, expected one of: jobs"#
        );

        let one = EnqueueRequest::One(JobSpec::new("q", "k").priority(5000));
        assert_eq!(
            lines(one.validate()),
            ["$.priority: must be between -1000 and 1000, got 5000"]
        );
        let batch = EnqueueRequest::Batch(vec![
            JobSpec::new("q", "k"),
            JobSpec::new("q", "k").priority(5000),
        ]);
        assert_eq!(
            lines(batch.validate()),
            ["$.jobs[1].priority: must be between -1000 and 1000, got 5000"]
        );
        assert_eq!(
            lines(EnqueueRequest::Batch(vec![]).validate()),
            ["$.jobs: must contain at least one job"]
        );
    }

    #[test]
    fn enqueue_response_round_trips() {
        let resp = EnqueueResponse {
            results: vec![EnqueueResult {
                id: "01HQ3K9Z8N6V2T4R5S7W0X1Y2Z".parse().unwrap(),
                status: EnqueueStatus::Duplicate,
                state: JobState::Running,
            }],
        };
        assert_eq!(
            resp.to_json(),
            json!({"results": [{"id": "01HQ3K9Z8N6V2T4R5S7W0X1Y2Z", "status": "duplicate", "state": "running"}]})
        );
        assert_eq!(EnqueueResponse::from_json(&resp.to_json()), Ok(resp));
    }

    #[test]
    fn lease_request_defaults_and_limits() {
        let req = LeaseRequest::from_json(&json!({"worker": "w1", "queues": ["emails"]})).unwrap();
        assert_eq!(
            req,
            LeaseRequest::new(
                WorkerId::from_static("w1"),
                vec![QueueName::from_static("emails")]
            )
        );
        assert_eq!(
            req.to_json(),
            json!({"worker": "w1", "queues": ["emails"], "max_jobs": 1, "lease_for": "30s"})
        );
        let full = req
            .clone()
            .max_jobs(10)
            .lease_for(Duration::from_secs(120))
            .wait(Duration::from_secs(20));
        assert_eq!(LeaseRequest::from_json(&full.to_json()), Ok(full.clone()));
        assert!(full.validate().is_ok());

        let mut bad = LeaseRequest::new(WorkerId::from_static("w1"), vec![])
            .max_jobs(0)
            .lease_for(Duration::from_millis(100))
            .wait(Duration::from_secs(600));
        assert_eq!(
            lines(bad.validate()),
            [
                "$.queues: must name at least one queue",
                "$.max_jobs: must be between 1 and 100, got 0",
                "$.lease_for: must be between 1s and 1h, got 100ms",
                "$.wait: must be at most 1m, got 10m",
            ]
        );
        bad = LeaseRequest::new(
            WorkerId::from_static("w1"),
            vec![QueueName::from_static("a"), QueueName::from_static("a")],
        );
        assert_eq!(lines(bad.validate()), ["$.queues[1]: duplicate queue a"]);
    }

    #[test]
    fn heartbeat_round_trips_and_validates_progress() {
        let req = HeartbeatRequest {
            token: token(),
            extend_by: Some(Duration::from_secs(60)),
            progress: Some(Progress::percent(10.0)),
        };
        let v = req.to_json();
        assert_eq!(
            v,
            json!({"token": "00000000000000a1", "extend_by": "1m", "progress": {"percent": 10.0}})
        );
        assert_eq!(HeartbeatRequest::from_json(&v), Ok(req));
        let bad = HeartbeatRequest {
            progress: Some(Progress::percent(-1.0)),
            ..HeartbeatRequest::new(token())
        };
        assert_eq!(
            lines(bad.validate()),
            ["$.progress.percent: must be between 0 and 100, got -1"]
        );
        let err = HeartbeatRequest::from_json(&json!({"token": "zz"})).unwrap_err();
        assert_eq!(
            err.to_string(),
            r#"$.token: invalid lease token "zz": expected 16 hex digits"#
        );
        let resp = HeartbeatResponse {
            expires_at: Timestamp::from_millis(60_000),
            cancel_requested: true,
        };
        assert_eq!(HeartbeatResponse::from_json(&resp.to_json()), Ok(resp));
    }

    #[test]
    fn complete_keeps_null_results_apart_from_missing_ones() {
        let with_null =
            CompleteRequest::from_json(&json!({"token": "00000000000000a1", "result": null}))
                .unwrap();
        assert_eq!(with_null.result, Some(Value::Null));
        assert_eq!(with_null.to_json()["result"], Value::Null);
        let without = CompleteRequest::from_json(&json!({"token": "00000000000000a1"})).unwrap();
        assert_eq!(without, CompleteRequest::new(token()));
        assert!(without.to_json().get("result").is_none());
        let big = CompleteRequest {
            result: Some(json!("x".repeat(MAX_RESULT_BYTES))),
            ..without
        };
        assert_eq!(
            lines(big.validate()),
            ["$.result: is 1048578 bytes as JSON; the limit is 1048576"]
        );
    }

    #[test]
    fn fail_request_defaults_to_retryable_and_builds_the_error_record() {
        let req =
            FailRequest::from_json(&json!({"token": "00000000000000a1", "error": "boom"})).unwrap();
        assert_eq!(req, FailRequest::new(token(), "boom"));
        let full = FailRequest {
            kind: Some("http_503".into()),
            retryable: false,
            retry_in: Some(Duration::from_secs(90)),
            ..req
        };
        assert_eq!(FailRequest::from_json(&full.to_json()), Ok(full.clone()));
        let worker = WorkerId::from_static("w1");
        let e = full.to_job_error(Timestamp::from_millis(5), 2, Some(worker.clone()));
        assert_eq!(
            (e.attempt, e.kind.as_deref(), e.retryable, e.worker),
            (2, Some("http_503"), false, Some(worker))
        );
        let huge = FailRequest::new(token(), "e".repeat(MAX_ERROR_MESSAGE_BYTES * 2));
        assert!(
            huge.validate().is_ok(),
            "long messages are truncated, not rejected"
        );
        assert!(
            huge.to_job_error(Timestamp::EPOCH, 1, None).message.len() <= MAX_ERROR_MESSAGE_BYTES
        );
        let bad = FailRequest {
            kind: Some("not ok".into()),
            retry_in: Some(Duration::from_secs(400 * 86_400)),
            ..FailRequest::new(token(), "x")
        };
        assert_eq!(
            lines(bad.validate()),
            [
                "$.kind: ' ' at position 3 is not allowed (use letters, digits, '_', '.', ':', '/' or '-')",
                "$.retry_in: must be at most 365d, got 400d",
            ]
        );
    }
}
