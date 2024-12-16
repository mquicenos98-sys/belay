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
