//! `/v1/jobs`: enqueueing, listing, inspecting and managing jobs.
//!
//! `POST /v1/jobs` takes a single `JobSpec` or `{"jobs": [...]}` and
//! answers one result per spec: `201 Created` when at least one job was
//! created (with `Location` for a single spec), `200 OK` when every spec
//! matched an existing job through its uniqueness key. Jobs returned here
//! never carry lease tokens.
//!
//! In a cluster, a job lives on the node that owned its queue when it was
//! enqueued; calls naming a job by id can only be served there. A node
//! that does not have the job says so in its `not_found` message.

use belay_http::header::LOCATION;
use belay_http::{Request, Response, StatusCode};
use belay_proto::api::{ApiError, EnqueueRequest, EnqueueStatus, ErrorCode, JobFilter};
use belay_proto::JobId;

use crate::app::App;
use crate::extract::{self, only_params};
use crate::placement::{self, Split};
use crate::reply;

pub(super) fn enqueue(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    only_params(req, &[])?;
    let request: EnqueueRequest = extract::body(req)?;
    let queues = request.jobs().iter().map(|spec| &spec.queue);
    match placement::split(queues, |queue| app.engine.owner_of(queue)) {
        Split::Local => {}
        Split::Remote(owner) => return Ok(placement::redirect(req, &owner)),
        Split::Mixed { local, remote } => return Err(placement::spread_batch(&local, &remote)),
    }
    let single = matches!(request, EnqueueRequest::One(_));
    let response = app.engine.enqueue_request(request)?;
    let created = response
        .results
        .iter()
        .any(|r| matches!(r.status, EnqueueStatus::Created | EnqueueStatus::Replaced));
    if !created {
        return Ok(reply::ok(&response));
    }
    let mut answer = reply::json(StatusCode::CREATED, &response);
    if let (true, Some(result)) = (single, response.results.first()) {
        answer = answer.header(LOCATION, job_path(result.id));
    }
    Ok(answer)
}

pub(super) fn list(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    let filter = JobFilter::from_pairs(req.query.iter())?;
    if let Some(queue) = &filter.queue {
        if let Some(redirect) = placement::for_queue(&app.engine, req, queue) {
            return Ok(redirect);
        }
    }
    Ok(reply::ok(&app.engine.list_jobs(&filter)?))
}

pub(super) fn get(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    only_params(req, &[])?;
    let id = extract::job_id(req)?;
    let job = app.engine.get_job(&id).map_err(|e| locate(app, e))?;
    Ok(reply::ok(&job))
}

pub(super) fn delete(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    only_params(req, &[])?;
    let id = extract::job_id(req)?;
    app.engine.delete_job(&id).map_err(|e| locate(app, e))?;
    Ok(reply::no_content())
}

pub(super) fn cancel(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    only_params(req, &[])?;
    let id = extract::job_id(req)?;
    let job = app.engine.cancel_job(&id).map_err(|e| locate(app, e))?;
    Ok(reply::ok(&job))
}

pub(super) fn retry(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    only_params(req, &[])?;
    let id = extract::job_id(req)?;
    let job = app.engine.retry_job(&id).map_err(|e| locate(app, e))?;
    Ok(reply::ok(&job))
}

pub(super) fn promote(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    only_params(req, &[])?;
    let id = extract::job_id(req)?;
    let job = app.engine.promote_job(&id).map_err(|e| locate(app, e))?;
    Ok(reply::ok(&job))
}
