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

/// Cancels the run and its unfinished steps; answers with the run.
pub(super) fn cancel(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    only_params(req, &[])?;
    let id = extract::workflow_id(req)?;
    Ok(reply::ok(&app.engine.cancel_workflow(id)?))
}

#[cfg(test)]
mod tests {
    use belay_http::Method;

    use crate::routes::testing;

    const SPEC: &str =
        r#"{"name": "etl", "steps": [{"name": "a", "job": {"queue": "q", "kind": "k"}}]}"#;

    #[test]
    fn listings_take_state_name_and_order() {
        let engine = testing::engine();
        let router = testing::router(&engine);
        let get = |target: &str| testing::call(&router, testing::request(Method::Get, target, ""));
        let mut ids = Vec::new();
        for _ in 0..3 {
            let submit = testing::request(Method::Post, "/v1/workflows", SPEC);
            let (status, body) = testing::call(&router, submit);
            assert_eq!(status, 201, "{body}");
            ids.push(body["id"].as_str().expect("id").to_owned());
        }
        let (_, oldest_first) = get("/v1/workflows?order=asc&limit=2");
        assert_eq!(oldest_first["items"][0]["id"], ids[0].as_str());
        assert!(oldest_first["next_cursor"].as_str().is_some());
        let (_, none) = get("/v1/workflows?state=failed&name=etl");
        assert_eq!(none["items"].as_array().map(Vec::len), Some(0));
        let (status, body) = get("/v1/workflows?state=exploded");
        assert_eq!(status, 400);
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(message.starts_with("$.state: "), "{message}");
    }

    #[test]
    fn unknown_and_malformed_runs_are_refused() {
        let engine = testing::engine();
        let router = testing::router(&engine);
        let missing = testing::request(Method::Get, "/v1/workflows/01HQ3K9Z8N6V2T4R5S7W0X1Y2Z", "");
        assert_eq!(testing::call(&router, missing).0, 404);
        let malformed = testing::request(Method::Post, "/v1/workflows/not-an-id/cancel", "");
        assert_eq!(testing::call(&router, malformed).0, 400);
    }
}
