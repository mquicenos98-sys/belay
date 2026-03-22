//! The worker protocol: `POST /v1/lease` and the per-job heartbeat,
//! complete and fail calls.
//!
//! # Long polls
//!
//! A lease with `wait` is held open until a job is ready or the wait (at
//! most 60 s) runs out. The engine wakes waiting requests as soon as
//! something could make work available, so a job enqueued mid-wait is
//! handed out at once. The request occupies its connection's thread for
//! the whole wait (belay-http runs one thread per connection), so
//! `server.max_connections` must cover the idle workers polling at any
//! time plus ordinary traffic and event streams.
//!
//! To keep a shutdown from waiting out every poll, the wait is taken in
//! slices of [`ServerOptions::shutdown_poll`](crate::ServerOptions::shutdown_poll):
//! between slices the handler checks the server's shutdown signal and, if
//! it fired, answers with no jobs. Slicing costs nothing in latency, since
//! wakeups within a slice are immediate.

use std::time::{Duration, Instant};

use belay_http::{Request, Response, ShutdownSignal};
use belay_proto::api::{
    ApiError, CompleteRequest, FailRequest, HeartbeatRequest, LeaseRequest, LeaseResponse,
    LeasedJob,
};
use belay_proto::JobSource;

use super::jobs::locate;
use crate::app::App;
use crate::extract::{self, only_params};
use crate::placement::{self, Split, REMOTE_QUEUES_HEADER};
use crate::reply;

pub(super) fn lease(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    only_params(req, &[])?;
    let mut request: LeaseRequest = extract::body(req)?;
    let mut skipped = Vec::new();
    match placement::split(&request.queues, |queue| app.engine.owner_of(queue)) {
        Split::Local => {}
        Split::Remote(owner) => return Ok(placement::redirect(req, &owner)),
        Split::Mixed { local, remote } => {
            if local.is_empty() {
                // Nothing to serve here: send the worker to the owner of
                // the queue it prefers most.
                let (_, owner) = &remote[0];
                return Ok(placement::redirect(req, owner));
            }
            skipped = remote
                .into_iter()
                .map(|(queue, _)| queue.to_string())
                .collect();
            request.queues = local;
        }
    }
    let shutdown = req.extensions.get::<ShutdownSignal>().cloned();
    let jobs = long_poll(app, &request, shutdown.as_ref())?;
    let response = reply::ok(&LeaseResponse { jobs });
    if skipped.is_empty() {
        Ok(response)
    } else {
        Ok(response.header(REMOTE_QUEUES_HEADER, skipped.join(",")))
    }
}

/// Leases for `request`, waiting up to its `wait` in slices so that a
/// server shutdown ends the wait early (with no jobs).
fn long_poll(
    app: &App,
    request: &LeaseRequest,
    shutdown: Option<&ShutdownSignal>,
) -> Result<Vec<LeasedJob>, ApiError> {
    let total = request.wait.unwrap_or(Duration::ZERO);
    let deadline = Instant::now() + total;
    let slice = app.options.poll();
    let mut sliced = request.clone();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        sliced.wait = (!remaining.is_zero()).then(|| remaining.min(slice));
        let jobs = JobSource::lease(app.engine.as_ref(), &sliced)?;
        if !jobs.is_empty() || sliced.wait.is_none() {
            return Ok(jobs);
        }
        if shutdown.is_some_and(ShutdownSignal::is_triggered) {
            return Ok(Vec::new());
        }
    }
}

pub(super) fn heartbeat(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    only_params(req, &[])?;
    let id = extract::job_id(req)?;
    let request: HeartbeatRequest = extract::body(req)?;
    let answer =
        JobSource::heartbeat(app.engine.as_ref(), &id, &request).map_err(|e| locate(app, e))?;
    Ok(reply::ok(&answer))
}

pub(super) fn complete(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    only_params(req, &[])?;
    let id = extract::job_id(req)?;
    let request: CompleteRequest = extract::body(req)?;
    JobSource::complete(app.engine.as_ref(), &id, &request).map_err(|e| locate(app, e))?;
    Ok(reply::no_content())
}

pub(super) fn fail(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    only_params(req, &[])?;
    let id = extract::job_id(req)?;
    let request: FailRequest = extract::body(req)?;
    JobSource::fail(app.engine.as_ref(), &id, &request).map_err(|e| locate(app, e))?;
    Ok(reply::no_content())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    use belay_core::Engine;
    use belay_http::Method;
    use belay_proto::{JobSpec, QueueName, WorkerId};

    use crate::options::ServerOptions;
    use crate::routes::testing;

    fn fast_polls(engine: &Arc<Engine>) -> App {
        let options = ServerOptions {
            shutdown_poll: Duration::from_millis(10),
            ..ServerOptions::default()
        };
        testing::app(engine, options)
    }

    fn request(wait: Duration) -> LeaseRequest {
        LeaseRequest::new(
            WorkerId::from_static("w1"),
            vec![QueueName::from_static("emails")],
        )
        .wait(wait)
    }

    #[test]
    fn a_triggered_shutdown_cuts_a_long_poll_short() {
        let engine = testing::engine();
        let app = fast_polls(&engine);
        let shutdown = ShutdownSignal::new();
        shutdown.trigger();
        let started = Instant::now();
        let jobs = long_poll(&app, &request(Duration::from_secs(30)), Some(&shutdown)).unwrap();
        assert!(jobs.is_empty());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn the_whole_wait_is_honoured_across_slices() {
        let engine = testing::engine();
        let app = fast_polls(&engine);
        let started = Instant::now();
        let jobs = long_poll(&app, &request(Duration::from_millis(80)), None).unwrap();
        assert!(jobs.is_empty());
        assert!(started.elapsed() >= Duration::from_millis(80));
    }

    #[test]
    fn a_job_arriving_mid_wait_is_handed_out_at_once() {
        let engine = testing::engine();
        let app = Arc::new(fast_polls(&engine));
        let poll = {
            let app = Arc::clone(&app);
            thread::spawn(move || {
                let started = Instant::now();
                let jobs = long_poll(&app, &request(Duration::from_secs(20)), None);
                (jobs, started.elapsed())
            })
        };
        thread::sleep(Duration::from_millis(50));
        let id = engine.enqueue(JobSpec::new("emails", "send")).unwrap().id;
        let (jobs, elapsed) = poll.join().expect("no panic");
        let jobs = jobs.unwrap();
        assert_eq!(jobs[0].job.id, id);
        assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
    }

    #[test]
    fn a_single_node_serves_every_queue_without_the_remote_header() {
        let engine = testing::engine();
        engine.enqueue(JobSpec::new("b", "send")).unwrap();
        let router = testing::router(&engine);
        let mut req = testing::request(
            Method::Post,
            "/v1/lease",
            r#"{"worker": "w1", "queues": ["a", "b"], "max_jobs": 5}"#,
        );
        let response = belay_http::Handler::call(&router, &mut req);
        assert_eq!(response.status, 200);
        assert!(response.headers.get(REMOTE_QUEUES_HEADER).is_none());
    }

    #[test]
    fn results_need_a_body_with_the_token() {
        let engine = testing::engine();
        let id = engine.enqueue(JobSpec::new("emails", "send")).unwrap().id;
        let router = testing::router(&engine);
        let (status, body) = testing::call(
            &router,
            testing::request(Method::Post, &format!("/v1/jobs/{id}/complete"), ""),
        );
        assert_eq!(status, 400);
        assert_eq!(
            body["error"]["message"],
            "request body is empty; expected a JSON object"
        );
        let (status, body) = testing::call(
            &router,
            testing::request(Method::Post, &format!("/v1/jobs/{id}/fail"), "{}"),
        );
        assert_eq!(status, 400);
        assert!(body["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("token")));
    }
}
