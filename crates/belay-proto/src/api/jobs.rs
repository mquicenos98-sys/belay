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
