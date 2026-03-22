//! `GET /v1/workers`: the workers this node has heard from, built from
//! their lease and heartbeat traffic. A worker counts as lost after
//! `engine.worker_timeout` of silence and is listed for
//! `engine.worker_retention` more; a worker polling several nodes of a
//! cluster shows up on each of them.

use belay_http::{Request, Response};
use belay_proto::api::ApiError;

use crate::app::App;
use crate::extract;
use crate::reply;

pub(super) fn list(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    let paging = extract::paging(req, &[])?;
    Ok(reply::ok(&app.engine.workers(&paging)?))
}

#[cfg(test)]
mod tests {
    use belay_http::Method;
    use belay_proto::api::LeaseRequest;
    use belay_proto::{JobSource, QueueName, WorkerId};

    use crate::routes::testing;

    #[test]
    fn workers_appear_once_they_lease_and_page_by_id() {
        let engine = testing::engine();
        for id in ["w2", "w1", "w3"] {
            let worker = WorkerId::new(id).expect("valid worker id");
            let request = LeaseRequest::new(worker, vec![QueueName::from_static("emails")]);
            engine.lease(&request).expect("lease");
        }
        let router = testing::router(&engine);
        let (_, first) = testing::call(
            &router,
            testing::request(Method::Get, "/v1/workers?limit=2", ""),
        );
        assert_eq!(first["items"][0]["id"], "w1");
        assert_eq!(first["items"][1]["id"], "w2");
        let cursor = first["next_cursor"].as_str().expect("a second page");
        let cursor = belay_http::percent::encode_component(cursor);
        let (_, rest) = testing::call(
            &router,
            testing::request(Method::Get, &format!("/v1/workers?cursor={cursor}"), ""),
        );
        assert_eq!(rest["items"][0]["id"], "w3");
        assert!(rest["next_cursor"].is_null());
    }
}
