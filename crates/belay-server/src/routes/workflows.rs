//! `/v1/workflows`: multi-step runs (DAGs of jobs).
//!
//! A run's steps are enqueued on the node that accepted the run, so a run
//! is served by that node alone; there is no redirect. `GET
//! /v1/workflows/:id?details=true` adds the failure message of each failed
//! step (`{"workflow": {...}, "errors": {"step": "..."}}`).

use belay_core::WorkflowFilter;
use belay_http::{Request, Response};
use belay_proto::api::{ApiError, SortOrder};
use belay_proto::workflow::WorkflowState;
use belay_proto::WorkflowSpec;

use crate::app::App;
use crate::extract::{self, only_params};
use crate::reply;

pub(super) fn submit(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    only_params(req, &[])?;
    let spec: WorkflowSpec = extract::body(req)?;
    let workflow = app.engine.submit_workflow(spec)?;
    Ok(reply::created(
        &workflow,
        &format!("/v1/workflows/{}", workflow.id),
    ))
}

/// Runs newest first; `?state=`, `?name=` and `?order=asc` narrow and
/// reorder the listing.
pub(super) fn list(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    let paging = extract::paging(req, &["state", "name", "order"])?;
    let filter = WorkflowFilter {
        state: extract::query_value::<WorkflowState>(req, "state")?,
        name: req
            .query
            .get("name")
            .filter(|n| !n.is_empty())
            .map(str::to_owned),
        cursor: paging.cursor,
        limit: paging.limit.unwrap_or(0),
        order: extract::query_value::<SortOrder>(req, "order")?.unwrap_or_default(),
    };
    Ok(reply::ok(&app.engine.list_workflows(&filter)?))
}

pub(super) fn get(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    only_params(req, &["details"])?;
    let id = extract::workflow_id(req)?;
    if extract::query_flag(req, "details")? {
        Ok(reply::ok(&app.engine.workflow_details(id)?))
    } else {
        Ok(reply::ok(&app.engine.get_workflow(id)?))
    }
}
