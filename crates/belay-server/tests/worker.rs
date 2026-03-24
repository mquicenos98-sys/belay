//! The worker protocol over HTTP: leasing (including long polls),
//! heartbeats, results, stale tokens, the client as a `JobSource`, and the
//! worker runtime draining a queue through all of it.

mod common;

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use belay_json::json;
use belay_proto::api::{CompleteRequest, ErrorCode, FailRequest, HeartbeatRequest, LeaseRequest};
use belay_proto::{JobId, JobSource, JobSpec, JobState, QueueName, WorkerId};
use belay_worker::{handler_fn, HandlerError, Outcome, Registry, Worker, WorkerConfig};
use common::{api_error, eventually, json, TestServer};

fn lease_request(queues: &[&'static str]) -> LeaseRequest {
    LeaseRequest::new(
        WorkerId::from_static("w1"),
        queues.iter().map(|q| QueueName::from_static(q)).collect(),
    )
}

#[test]
fn a_job_goes_from_lease_to_success() {
    let server = TestServer::start();
    let client = server.client();
    let id = client
        .jobs()
        .enqueue(&JobSpec::new("emails", "send"))
        .unwrap()
        .id;
    let leased = client.jobs().lease(&lease_request(&["emails"])).unwrap();
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].job.id, id);
    assert_eq!(leased[0].job.state, JobState::Running);

    let mut beat = HeartbeatRequest::new(leased[0].token);
    beat.extend_by = Some(Duration::from_secs(60));
    let answer = client.jobs().heartbeat(id, &beat).unwrap();
    assert!(!answer.cancel_requested);

    let mut done = CompleteRequest::new(leased[0].token);
    done.result = Some(json!({"sent": true}));
    client.jobs().complete(id, &done).unwrap();
    let job = client.jobs().get(id).unwrap();
    assert_eq!(job.state, JobState::Succeeded);
    assert_eq!(job.result, Some(json!({"sent": true})));
    // Read endpoints never show the lease token.
    let raw = server.get(&format!("/v1/jobs/{id}"), None);
    assert!(!String::from_utf8_lossy(&raw.body).contains(&leased[0].token.to_string()));
}

#[test]
fn complete_and_fail_answer_no_content() {
    let server = TestServer::start();
    let client = server.client();
    let id = client
        .jobs()
        .enqueue(&JobSpec::new("emails", "send").max_attempts(1))
        .unwrap()
        .id;
    let leased = client.jobs().lease(&lease_request(&["emails"])).unwrap();
    let body = belay_json::to_string_of(&FailRequest::new(leased[0].token, "smtp down"));
    let response = server.post(&format!("/v1/jobs/{id}/fail"), &body, None);
    assert_eq!(response.status, 204);
    assert!(response.body.is_empty());
    assert_eq!(client.jobs().get(id).unwrap().state, JobState::Dead);
}

#[test]
fn stale_tokens_lose_the_lease() {
    let server = TestServer::start();
    let client = server.client();
    let id = client
        .jobs()
        .enqueue(&JobSpec::new("emails", "send"))
        .unwrap()
        .id;
    let leased = client.jobs().lease(&lease_request(&["emails"])).unwrap();
    client.jobs().cancel(id).unwrap();
    let beat = client
        .jobs()
        .heartbeat(id, &HeartbeatRequest::new(leased[0].token))
        .unwrap();
    assert!(beat.cancel_requested);

    let wrong = format!(r#"{{"token": "{}"}}"#, "0123456789abcdef");
    let response = server.post(&format!("/v1/jobs/{id}/complete"), &wrong, None);
    assert_eq!(response.status, 409);
    assert_eq!(api_error(&response).code, ErrorCode::LeaseLost);
}

#[test]
fn a_long_poll_wakes_up_when_a_job_arrives() {
    let server = TestServer::start();
    let client = server.client();
    let waiting = {
        let client = client.clone();
        thread::spawn(move || {
            let started = Instant::now();
            let request = lease_request(&["emails"]).wait(Duration::from_secs(20));
            let jobs = client.jobs().lease(&request).expect("lease");
            (jobs, started.elapsed())
        })
    };
    // Let the request reach the server and start waiting.
    assert!(common::eventually(Duration::from_secs(5), || {
        server
            .engine
            .render_metrics()
            .contains("belay_lease_waiting 1")
    }));
    let id = client
        .jobs()
        .enqueue(&JobSpec::new("emails", "send"))
        .unwrap()
        .id;
    let (jobs, elapsed) = waiting.join().expect("no panic");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].job.id, id);
    assert!(elapsed < Duration::from_secs(10), "took {elapsed:?}");
}

#[test]
fn a_long_poll_with_nothing_to_do_answers_empty_when_the_wait_ends() {
    let server = TestServer::start();
    let started = Instant::now();
    let response = server.post(
        "/v1/lease",
        r#"{"worker": "w1", "queues": ["empty"], "wait": "300ms"}"#,
        None,
    );
    let elapsed = started.elapsed();
    assert_eq!(response.status, 200);
    assert_eq!(json(&response)["jobs"], json!([]));
    assert!(elapsed >= Duration::from_millis(250), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
}

#[test]
fn lease_requests_are_validated() {
    let server = TestServer::start();
    let error = api_error(&server.post("/v1/lease", r#"{"worker": "w1", "queues": []}"#, None));
    assert_eq!(error.message, "$.queues: must name at least one queue");
    let too_long = server.post(
        "/v1/lease",
        r#"{"worker": "w1", "queues": ["q"], "wait": "5m"}"#,
        None,
    );
    assert_eq!(too_long.status, 400);
}

#[test]
fn the_client_serves_as_a_job_source() {
    let server = TestServer::start();
    let source: Arc<dyn JobSource> = Arc::new(server.client());
    server
        .client()
        .jobs()
        .enqueue(&JobSpec::new("emails", "send"))
        .unwrap();
    let leased = source.lease(&lease_request(&["emails"])).unwrap();
    let id = leased[0].job.id;
    source
        .complete(&id, &CompleteRequest::new(leased[0].token))
        .unwrap();
    assert_eq!(
        server.engine.get_job(&id).unwrap().state,
        JobState::Succeeded
    );
    // The worker is now known to the server.
    let workers = server.client().workers().unwrap();
    assert_eq!(workers.len(), 1);
    assert_eq!(workers[0].id.as_str(), "w1");
}

#[test]
fn a_worker_runtime_drains_a_queue_over_http() {
    let server = TestServer::start();
    let registry = Registry::new()
        .register(
            "double",
            handler_fn(|ctx| {
                let n = ctx.payload()["n"].as_i64().unwrap_or_default();
                Ok(Outcome::with(json!({ "doubled": n * 2 })))
            }),
        )
        .register(
            "broken",
            handler_fn(|_| Err(HandlerError::fatal("cannot be done"))),
        );
    let config = WorkerConfig::new(
        WorkerId::from_static("runtime-1"),
        vec![QueueName::from_static("math")],
    )
    .concurrency(4)
    .poll_wait(Duration::from_millis(200))
    .poll_interval(Duration::from_millis(10));
    let source: Arc<dyn JobSource> = Arc::new(server.client());
    let worker = Worker::new(source, registry, config)
        .expect("valid worker")
        .spawn()
        .expect("worker starts");

    let client = server.client();
    let doubled: Vec<(i64, JobId)> = (0..10)
        .map(|n| {
            let spec = JobSpec::new("math", "double").payload(json!({ "n": n }));
            (n, client.jobs().enqueue(&spec).unwrap().id)
        })
        .collect();
    let broken = client
        .jobs()
        .enqueue(&JobSpec::new("math", "broken"))
        .unwrap()
        .id;
    let finished = eventually(Duration::from_secs(10), || {
        doubled.iter().map(|(_, id)| *id).chain([broken]).all(|id| {
            server
                .engine
                .get_job(&id)
                .is_ok_and(|j| j.state.is_terminal())
        })
    });
    let report = worker.stop();
    assert!(finished, "the worker did not finish the queue");
    assert!(report.drained);
    assert_eq!(report.stats.leased, 11);
    assert_eq!((report.stats.succeeded, report.stats.failed), (10, 1));

    for (n, id) in doubled {
        let job = client.jobs().get(id).unwrap();
        assert_eq!(job.state, JobState::Succeeded);
        assert_eq!(job.result, Some(json!({ "doubled": n * 2 })));
    }
    let dead = client.jobs().get(broken).unwrap();
    assert_eq!(dead.state, JobState::Dead);
    assert_eq!(
        dead.errors.last().map(|e| e.message.as_str()),
        Some("cannot be done")
    );
}
