//! `/v1/jobs` and `/v1/lease`: enqueueing, inspection, management, and the
//! worker protocol (lease, heartbeat, complete, fail), which also backs the
//! client's [`JobSource`] implementation.

use belay_json::{ObjectBuilder, Value};
use belay_proto::api::{
    ApiError, CompleteRequest, EnqueueResponse, EnqueueResult, FailRequest, HeartbeatRequest,
    HeartbeatResponse, JobFilter, LeaseRequest, LeaseResponse, LeasedJob, Page,
};
use belay_proto::{Job, JobId, JobSource, JobSpec};

use crate::client::{segment, Call, Client};
use crate::error::{DecodeError, Result};
use crate::pager::Paginator;
use crate::retry::{is_repeatable_enqueue, Safety};

/// Job calls; see [`Client::jobs`].
#[derive(Clone, Copy, Debug)]
pub struct Jobs<'a> {
    client: &'a Client,
}

fn job_path(id: JobId, action: &str) -> String {
    let id = segment(&id.to_string());
    if action.is_empty() {
        format!("/v1/jobs/{id}")
    } else {
        format!("/v1/jobs/{id}/{action}")
    }
}

impl<'a> Jobs<'a> {
    pub(crate) fn new(client: &'a Client) -> Jobs<'a> {
        Jobs { client }
    }

    /// Enqueues one job (`POST /v1/jobs` with the bare spec).
    ///
    /// The result says whether a job was created or, for a spec with a
    /// uniqueness key that is already held, which existing job holds it.
    /// Retried on failure only when the spec's key makes a repeat harmless
    /// (see [`is_repeatable_enqueue`]).
    pub fn enqueue(&self, spec: &JobSpec) -> Result<EnqueueResult> {
        let safety = if is_repeatable_enqueue(spec) {
            Safety::Repeatable
        } else {
            Safety::NotRepeatable
        };
        let call = Call::post("/v1/jobs".to_owned(), safety).json(spec);
        let endpoint = call.endpoint();
        let mut results = self.client.fetch::<EnqueueResponse>(call)?.results;
        if results.len() != 1 {
            return Err(DecodeError::new(
                endpoint,
                format!("expected 1 result for 1 job, got {}", results.len()),
            )
            .into());
        }
        Ok(results.remove(0))
    }

    /// Enqueues several jobs in one request (`POST /v1/jobs` with
    /// `{"jobs": [...]}`), returning one result per spec, in order.
    ///
    /// The server accepts up to 1000 specs per request and checks them all
    /// before enqueueing any. An empty slice returns an empty list without
    /// a request. Retried only when every spec is
    /// [repeatable](is_repeatable_enqueue).
    pub fn enqueue_batch(&self, specs: &[JobSpec]) -> Result<Vec<EnqueueResult>> {
        if specs.is_empty() {
            return Ok(Vec::new());
        }
        let safety = if specs.iter().all(is_repeatable_enqueue) {
            Safety::Repeatable
        } else {
            Safety::NotRepeatable
        };
        let body = batch_body(specs);
        let call = Call::post("/v1/jobs".to_owned(), safety).json(&body);
        let endpoint = call.endpoint();
        let results = self.client.fetch::<EnqueueResponse>(call)?.results;
        if results.len() != specs.len() {
            return Err(DecodeError::new(
                endpoint,
                format!(
                    "expected {} results for {} jobs, got {}",
                    specs.len(),
                    specs.len(),
                    results.len()
                ),
            )
            .into());
        }
        Ok(results)
    }

    /// The job `id` (`GET /v1/jobs/:id`). Lease tokens are redacted.
    pub fn get(&self, id: JobId) -> Result<Job> {
        self.client.fetch(Call::get(job_path(id, "")))
    }

    /// One page of jobs matching `filter` (`GET /v1/jobs?...`), newest
    /// first unless the filter says otherwise.
    pub fn list(&self, filter: &JobFilter) -> Result<Page<Job>> {
        self.client
            .fetch(Call::get("/v1/jobs".to_owned()).query(filter.to_query()))
    }

    /// Every job matching `filter`, following cursors page by page as the
    /// iterator is consumed. The filter's `cursor` is the starting point
    /// and its `limit` the page size.
    #[must_use]
    pub fn list_all(&self, mut filter: JobFilter) -> Paginator<Job> {
        let cursor = filter.cursor.take();
        Paginator::new(
            self.client.clone(),
            "/v1/jobs".to_owned(),
            filter.to_query(),
            cursor,
        )
    }

    /// Cancels `id` (`POST /v1/jobs/:id/cancel`) and returns it. A waiting
    /// job is cancelled at once; a running one is flagged, and its worker
    /// learns about it on its next heartbeat.
    pub fn cancel(&self, id: JobId) -> Result<Job> {
        self.client
            .fetch(Call::post(job_path(id, "cancel"), Safety::Repeatable))
    }

    /// Sends a dead or cancelled job back to `ready`
    /// (`POST /v1/jobs/:id/retry`). Not retried automatically: a repeat
    /// after the job ran and died again would run it once more.
    pub fn retry(&self, id: JobId) -> Result<Job> {
        self.client
            .fetch(Call::post(job_path(id, "retry"), Safety::NotRepeatable))
    }

    /// Makes a scheduled or retrying job ready now
    /// (`POST /v1/jobs/:id/promote`).
    pub fn promote(&self, id: JobId) -> Result<Job> {
        self.client
            .fetch(Call::post(job_path(id, "promote"), Safety::NotRepeatable))
    }

    /// Deletes a finished job (`DELETE /v1/jobs/:id`); unfinished jobs are
    /// refused with `failed_precondition`.
    pub fn delete(&self, id: JobId) -> Result<()> {
        self.client.execute(Call::delete(job_path(id, "")))
    }

    /// Leases up to `req.max_jobs` jobs (`POST /v1/lease`).
    ///
    /// With `req.wait` set the server holds the request until a job is
    /// ready or the wait runs out, so the read timeout for this call is the
    /// client's `timeout` plus the wait. An empty list means nothing became
    /// ready in time. Never retried: a lost answer would strand the leased
    /// jobs until their leases expire.
    pub fn lease(&self, req: &LeaseRequest) -> Result<Vec<LeasedJob>> {
        let mut call = Call::post("/v1/lease".to_owned(), Safety::NotRepeatable).json(req);
        if let Some(wait) = req.wait {
            call = call.long_poll(wait);
        }
        Ok(self.client.fetch::<LeaseResponse>(call)?.jobs)
    }

    /// Renews the lease on `id` and records progress
    /// (`POST /v1/jobs/:id/heartbeat`). Check
    /// [`cancel_requested`](HeartbeatResponse::cancel_requested) in the
    /// answer; a stale token fails with `lease_lost`.
    pub fn heartbeat(&self, id: JobId, req: &HeartbeatRequest) -> Result<HeartbeatResponse> {
        self.client
            .fetch(Call::post(job_path(id, "heartbeat"), Safety::Repeatable).json(req))
    }

    /// Reports success (`POST /v1/jobs/:id/complete`).
    pub fn complete(&self, id: JobId, req: &CompleteRequest) -> Result<()> {
        self.client
            .execute(Call::post(job_path(id, "complete"), Safety::Repeatable).json(req))
    }

    /// Reports a failed attempt (`POST /v1/jobs/:id/fail`); the server
    /// schedules a retry or marks the job dead.
    pub fn fail(&self, id: JobId, req: &FailRequest) -> Result<()> {
        self.client
            .execute(Call::post(job_path(id, "fail"), Safety::Repeatable).json(req))
    }
}

/// The worker protocol over HTTP, so a belay-worker runtime can lease from
/// a remote server exactly as from the embedded engine. Every call maps to
/// the matching [`Jobs`] method, with errors converted by
/// [`Error::into_api_error`](crate::Error::into_api_error): an unreachable
/// server surfaces as `unavailable`, which workers treat as "back off and
/// try again", and `lease_lost` passes through unchanged.
///
/// ```no_run
/// use std::sync::Arc;
/// use belay_client::Client;
/// use belay_proto::JobSource;
///
/// let source: Arc<dyn JobSource> = Arc::new(Client::from_env()?);
/// # Ok::<(), belay_client::Error>(())
/// ```
impl JobSource for Client {
    fn lease(&self, req: &LeaseRequest) -> Result<Vec<LeasedJob>, ApiError> {
        self.jobs().lease(req).map_err(ApiError::from)
    }

    fn heartbeat(&self, id: &JobId, req: &HeartbeatRequest) -> Result<HeartbeatResponse, ApiError> {
        self.jobs().heartbeat(*id, req).map_err(ApiError::from)
    }

    fn complete(&self, id: &JobId, req: &CompleteRequest) -> Result<(), ApiError> {
        self.jobs().complete(*id, req).map_err(ApiError::from)
    }

    fn fail(&self, id: &JobId, req: &FailRequest) -> Result<(), ApiError> {
        self.jobs().fail(*id, req).map_err(ApiError::from)
    }
}

/// `{"jobs": [...]}` without copying the specs into an
/// [`EnqueueRequest`](belay_proto::api::EnqueueRequest).
fn batch_body(specs: &[JobSpec]) -> Value {
    ObjectBuilder::new().field("jobs", specs).build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_json::ToJson;
    use belay_proto::api::EnqueueRequest;

    #[test]
    fn batch_body_matches_the_proto_wire_form() {
        let specs = vec![
            JobSpec::new("emails", "send").priority(5),
            JobSpec::new("sms", "send").tag("urgent"),
        ];
        assert_eq!(
            batch_body(&specs),
            EnqueueRequest::Batch(specs.clone()).to_json()
        );
    }

    #[test]
    fn job_paths_name_the_action() {
        let id: JobId = "01HQ3K9Z8N6V2T4R5S7W0X1Y2Z".parse().unwrap();
        assert_eq!(job_path(id, ""), "/v1/jobs/01HQ3K9Z8N6V2T4R5S7W0X1Y2Z");
        assert_eq!(
            job_path(id, "heartbeat"),
            "/v1/jobs/01HQ3K9Z8N6V2T4R5S7W0X1Y2Z/heartbeat"
        );
    }
}
