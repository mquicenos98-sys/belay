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
/// Queues are tried in the order given, which lets a worker express
/// preference. With `wait`, the server holds the request open until a job
/// arrives or the wait runs out (long polling), then answers with whatever
/// it has, possibly nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseRequest {
    /// Who is asking.
    pub worker: WorkerId,
    /// Where to look, in order of preference.
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
