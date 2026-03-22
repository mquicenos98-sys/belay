//! `/v1/queues`: status, configuration, pausing, purging and deletion.
//!
//! Queues come into existence when a job is enqueued into them or when
//! they are configured, paused or resumed; there is no separate "create".
//! Every call naming a queue is redirected to its owner in a cluster; the
//! listing shows the queues this node knows.

use belay_http::{Request, Response};
use belay_proto::api::ApiError;
use belay_proto::{JobState, QueueConfig};

use crate::app::App;
use crate::extract::{self, only_params};
use crate::placement;
use crate::reply;

pub(super) fn list(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    let paging = extract::paging(req, &[])?;
    Ok(reply::ok(&app.engine.list_queues(&paging)?))
}

/// Runs `op` on the queue named in the path, unless another node owns it.
fn on_queue(
    app: &App,
    req: &mut Request,
    params: &[&str],
    op: impl FnOnce(&App, &Request, &belay_proto::QueueName) -> Result<Response, ApiError>,
) -> Result<Response, ApiError> {
    only_params(req, params)?;
    let name = extract::queue_name(req)?;
    if let Some(redirect) = placement::for_queue(&app.engine, req, &name) {
        return Ok(redirect);
    }
    op(app, req, &name)
}

pub(super) fn get(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    on_queue(app, req, &[], |app, _, name| {
        Ok(reply::ok(&app.engine.queue_info(name)?))
    })
}

pub(super) fn set_config(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    on_queue(app, req, &[], |app, req, name| {
        let config: QueueConfig = extract::body(req)?;
        Ok(reply::ok(&app.engine.set_queue_config(name, config)?))
    })
}

pub(super) fn pause(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    on_queue(app, req, &[], |app, _, name| {
        Ok(reply::ok(&app.engine.pause_queue(name)?))
    })
}

pub(super) fn resume(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    on_queue(app, req, &[], |app, _, name| {
        Ok(reply::ok(&app.engine.resume_queue(name)?))
    })
}

/// `?state=` limits the purge to one state; without it every job but the
/// running ones goes.
pub(super) fn purge(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    on_queue(app, req, &["state"], |app, req, name| {
        let state: Option<JobState> = extract::query_value(req, "state")?;
        Ok(reply::ok(&app.engine.purge_queue(name, state)?))
    })
}

/// Refused while the queue has unfinished jobs, unless `?force=true`.
pub(super) fn delete(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    on_queue(app, req, &["force"], |app, req, name| {
        let force = extract::query_flag(req, "force")?;
        Ok(reply::ok(&app.engine.delete_queue(name, force)?))
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use belay_http::Method;
    use belay_proto::JobSpec;

    use crate::routes::testing;

    #[test]
    fn purging_by_state_leaves_other_states_alone() {
        let engine = testing::engine();
        engine.enqueue(JobSpec::new("emails", "now")).unwrap();
        let later = JobSpec::new("emails", "later").delay(Duration::from_secs(60));
        engine.enqueue(later).unwrap();
        let router = testing::router(&engine);
        let purge =
            |target: &str| testing::call(&router, testing::request(Method::Post, target, ""));
        let (status, body) = purge("/v1/queues/emails/purge?state=scheduled");
        assert_eq!(status, 200);
        assert_eq!(body["purged"], 1);
        let (_, info) = testing::call(
            &router,
            testing::request(Method::Get, "/v1/queues/emails", ""),
        );
        assert_eq!(info["counts"]["ready"], 1);
        let (_, all) = purge("/v1/queues/emails/purge");
        assert_eq!(all["purged"], 1);
    }

    #[test]
    fn pausing_an_unknown_queue_creates_it() {
        let engine = testing::engine();
        let router = testing::router(&engine);
        let (status, body) = testing::call(
            &router,
            testing::request(Method::Post, "/v1/queues/fresh/pause", ""),
        );
        assert_eq!(status, 200);
        assert_eq!(body["paused"], true);
        let (status, _) = testing::call(
            &router,
            testing::request(Method::Get, "/v1/queues/ghost", ""),
        );
        assert_eq!(status, 404);
    }

    #[test]
    fn configurations_are_validated() {
        let engine = testing::engine();
        let router = testing::router(&engine);
        let put = |body: &str| {
            testing::call(
                &router,
                testing::request(Method::Put, "/v1/queues/emails/config", body),
            )
        };
        let (status, body) = put(r#"{"max_concurrency": 4}"#);
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["config"]["max_concurrency"], 4);
        let (status, body) = put(r#"{"max_concurency": 4}"#);
        assert_eq!(status, 400);
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains("max_concurency"), "{message}");
    }
}
