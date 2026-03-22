//! Retries, error mapping, cluster redirects and long-poll timeouts,
//! observed from the server side by counting attempts.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use belay_client::{Client, Error};
use belay_common::log::CaptureLogger;
use belay_http::{Request, Response, Router, Server, StatusCode};
use belay_json::json;
use belay_proto::api::{
    ApiError, EnqueueResponse, EnqueueResult, EnqueueStatus, ErrorCode, HeartbeatRequest,
    HeartbeatResponse, JobFilter, LeaseRequest, LeaseResponse, Page,
};
use belay_proto::{JobSpec, JobState, LeaseToken, QueueName, ScheduleId, UniqueScope, WorkerId};
use common::{
    fast_config, job_id, sample_job, server_config, t0, unreachable_config, Fake, Hangup,
};

fn unavailable() -> ApiError {
    ApiError::unavailable("node is draining")
}

fn created() -> EnqueueResponse {
    EnqueueResponse {
        results: vec![EnqueueResult {
            id: job_id(1),
            status: EnqueueStatus::Created,
            state: JobState::Ready,
        }],
    }
}

fn lease_request() -> LeaseRequest {
    LeaseRequest::new(
        WorkerId::from_static("w1"),
        vec![QueueName::from_static("emails")],
    )
}

// ---- retries ------------------------------------------------------------

#[test]
fn reads_are_retried_through_a_brief_outage() {
    let fake = Fake::start();
    fake.reply_error(unavailable());
    fake.reply_status(502);
    fake.reply_json(200, &sample_job(1));

    let job = fake
        .client()
        .jobs()
        .get(job_id(1))
        .expect("third attempt works");

    assert_eq!(job.id, job_id(1));
    assert_eq!(fake.count(), 3);
}

#[test]
fn the_retry_budget_ends_with_the_last_error() {
    let fake = Fake::start();
    for _ in 0..3 {
        fake.reply_error(unavailable());
    }
    let client = fake.client_with(|c| c.retries(2));

    let err = client.jobs().get(job_id(1)).unwrap_err();

    assert_eq!(err.api_error(), Some(&unavailable()));
    assert!(err.is_retryable(), "the caller may still try later");
    assert_eq!(fake.count(), 3, "one attempt plus two retries");
}

#[test]
fn zero_retries_means_one_attempt() {
    let fake = Fake::start();
    fake.reply_status(503);
    let client = fake.client_with(|c| c.retries(0));
    assert!(client.queues().list_all().next().expect("an item").is_err());
    assert_eq!(fake.count(), 1);
}

#[test]
fn client_and_server_errors_are_not_retried() {
    let fake = Fake::start();
    let client = fake.client();
    for error in [
        ApiError::not_found("job", job_id(1)),
        ApiError::unauthenticated("bad token"),
        ApiError::internal("bug"),
        ApiError::conflict("busy"),
    ] {
        fake.reply_error(error.clone());
        let err = client.jobs().get(job_id(1)).unwrap_err();
        assert_eq!(err.api_error(), Some(&error));
    }
    assert_eq!(fake.count(), 4);
}

#[test]
fn plain_enqueue_is_not_retried_after_the_server_saw_it() {
    let fake = Fake::start();
    fake.reply_error(unavailable());
    fake.reply_json(200, &created());

    let err = fake
        .client()
        .jobs()
        .enqueue(&JobSpec::new("emails", "send"))
        .unwrap_err();

    assert_eq!(err.code(), Some(ErrorCode::Unavailable));
    assert_eq!(fake.count(), 1, "a repeat could create a second job");
    assert_eq!(fake.pending_replies(), 1);
}

#[test]
fn enqueue_with_a_lasting_unique_key_is_retried() {
    let fake = Fake::start();
    fake.reply_status(503);
    fake.reply_json(200, &created());
    let spec = JobSpec::new("emails", "send").unique("user:42", UniqueScope::UntilFinished);

    fake.client()
        .jobs()
        .enqueue(&spec)
        .expect("second attempt works");

    let seen = fake.requests();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].body, seen[1].body, "the same spec is resent");
}

#[test]
fn a_batch_is_retried_only_when_every_spec_is_repeatable() {
    let fake = Fake::start();
    let unique = JobSpec::new("q", "k").unique("a", UniqueScope::UntilFinished);
    let plain = JobSpec::new("q", "k");
    fake.reply_status(503);
    let err = fake
        .client()
        .jobs()
        .enqueue_batch(&[unique.clone(), plain])
        .unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::Unavailable));
    assert_eq!(fake.count(), 1);
}

#[test]
fn lease_trigger_and_purge_are_never_repeated() {
    let fake = Fake::start();
    let client = fake.client();
    fake.reply_status(503);
    assert!(client.jobs().lease(&lease_request()).is_err());
    fake.reply_status(503);
    assert!(client
        .schedules()
        .trigger(&ScheduleId::from_static("nightly"))
        .is_err());
    fake.reply_status(504);
    assert!(client
        .queues()
        .purge(&QueueName::from_static("emails"), None)
        .is_err());
    fake.reply_status(502);
    assert!(client.jobs().retry(job_id(1)).is_err());
    assert_eq!(fake.count(), 4);
}

#[test]
fn heartbeats_ride_out_a_gateway_hiccup() {
    let fake = Fake::start();
    fake.reply_status(502);
    fake.reply_json(
        200,
        &HeartbeatResponse {
            expires_at: t0(),
            cancel_requested: false,
        },
    );
    let request = HeartbeatRequest::new(LeaseToken::new(7));
    fake.client()
        .jobs()
        .heartbeat(job_id(1), &request)
        .expect("retried heartbeat");
    assert_eq!(fake.count(), 2);
}

#[test]
fn rate_limited_reads_are_retried() {
    let fake = Fake::start();
    fake.reply_error(ApiError::resource_exhausted("slow down"));
    fake.reply_json(200, &Page::<belay_proto::Job>::last(vec![]));
    fake.client()
        .jobs()
        .list(&JobFilter::new())
        .expect("second attempt");
    assert_eq!(fake.count(), 2);
}

#[test]
fn retry_after_is_honoured_when_short() {
    let fake = Fake::start();
    fake.reply_with(|_| {
        Response::status(StatusCode::SERVICE_UNAVAILABLE).header("Retry-After", "0")
    });
    fake.reply_json(200, &sample_job(1));
    fake.client().jobs().get(job_id(1)).expect("retried");
    assert_eq!(fake.count(), 2);
}

#[test]
fn retry_after_beyond_the_timeout_is_not_waited_out() {
    let fake = Fake::start();
    fake.reply_with(|_| {
        Response::status(StatusCode::SERVICE_UNAVAILABLE).header("Retry-After", "3600")
    });
    let started = Instant::now();

    let err = fake.client().jobs().get(job_id(1)).unwrap_err();

    assert_eq!(err.code(), Some(ErrorCode::Unavailable));
    assert_eq!(fake.count(), 1);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "returned at once"
    );
}

#[test]
fn bodiless_proxy_errors_fall_back_to_the_status() {
    let fake = Fake::start();
    fake.reply_with(|_| {
        Response::bytes("text/html", b"<html>Bad Gateway</html>".to_vec())
            .with_status(StatusCode::BAD_GATEWAY)
    });
    let client = fake.client_with(|c| c.retries(0));

    let err = client.info().unwrap_err();

    let api = err.api_error().expect("an api error");
    assert_eq!(api.code, ErrorCode::Unavailable);
    assert_eq!(api.message, "HTTP 502: <html>Bad Gateway</html>");
}

#[test]
fn connections_that_never_open_are_retried_even_for_enqueue() {
    let client = Client::new(unreachable_config().retries(2)).expect("config");
    let capture = CaptureLogger::new();

    let err = capture
        .scope(|| client.jobs().enqueue(&JobSpec::new("emails", "send")))
        .unwrap_err();

    assert!(matches!(err, Error::Transport(_)), "{err}");
    assert!(err.is_retryable());
    let retries: Vec<_> = capture
        .records()
        .into_iter()
        .filter(|r| r.message.starts_with("retrying POST /v1/jobs"))
        .collect();
    assert_eq!(retries.len(), 2, "nothing was sent, so repeating is safe");
    assert_eq!(
        retries[1].message,
        "retrying POST /v1/jobs after attempt 2 failed"
    );
}

#[test]
fn probes_do_not_retry_connections_that_never_open() {
    let client = Client::new(unreachable_config()).expect("config");
    let capture = CaptureLogger::new();
    let err = capture.scope(|| client.health()).unwrap_err();
    assert!(matches!(err, Error::Transport(_)), "{err}");
    assert!(!capture.contains("retrying"));
}

#[test]
fn dropped_connections_are_retried_only_for_repeatable_calls() {
    let hangup = Hangup::start();
    let client = Client::new(fast_config(&hangup.url()).retries(2)).expect("config");

    let err = client.jobs().get(job_id(1)).unwrap_err();
    assert!(matches!(err, Error::Transport(_)), "{err}");
    assert_eq!(hangup.accepted(), 3);

    let err = client
        .jobs()
        .enqueue(&JobSpec::new("emails", "send"))
        .unwrap_err();
    assert!(matches!(err, Error::Transport(_)), "{err}");
    assert_eq!(hangup.accepted(), 4, "the server may have acted on it");
}

// ---- redirects ----------------------------------------------------------

/// A node that redirects everything to `target` with a 307.
fn redirector(target: String) -> belay_http::ServerHandle {
    let mut router = Router::new();
    let redirect = move |req: &mut Request| {
        Response::redirect(
            StatusCode::TEMPORARY_REDIRECT,
            &format!("{target}{}", req.target),
        )
    };
    router
        .get("/*rest", redirect.clone())
        .post("/*rest", redirect);
    Server::bind("127.0.0.1:0", server_config())
        .expect("bind")
        .spawn(router)
        .expect("spawn")
}

#[test]
fn redirects_to_the_owner_node_carry_the_token() {
    let owner = Fake::start();
    owner.reply_json(200, &sample_job(1));
    let node = redirector(owner.url());
    let client =
        Client::new(fast_config(&format!("http://{}", node.local_addr()))).expect("config");

    let job = client.jobs().get(job_id(1)).expect("served by the owner");

    assert_eq!(job.id, job_id(1));
    let seen = owner.only();
    assert_eq!(seen.call(), "GET /v1/jobs/:id");
    assert_eq!(seen.header("authorization"), Some("Bearer test-token"));
    node.shutdown_and_join();
}

#[test]
fn redirected_posts_keep_their_method_and_body() {
    let owner = Fake::start();
    owner.reply_json(200, &created());
    let node = redirector(owner.url());
    let client =
        Client::new(fast_config(&format!("http://{}", node.local_addr()))).expect("config");
    let spec = JobSpec::new("emails", "send").payload(json!({"user": 1}));

    client.jobs().enqueue(&spec).expect("enqueued on the owner");

    let seen = owner.only();
    assert_eq!(seen.call(), "POST /v1/jobs");
    assert_eq!(seen.json()["payload"], json!({"user": 1}));
    node.shutdown_and_join();
}

#[test]
fn redirect_loops_end_with_a_transport_error() {
    let hops = Arc::new(AtomicUsize::new(0));
    let mut router = Router::new();
    {
        let hops = Arc::clone(&hops);
        router.get("/*rest", move |req: &mut Request| {
            hops.fetch_add(1, Ordering::SeqCst);
            Response::redirect(StatusCode::TEMPORARY_REDIRECT, &req.target)
        });
    }
    let node = Server::bind("127.0.0.1:0", server_config())
        .expect("bind")
        .spawn(router)
        .expect("spawn");
    let client =
        Client::new(fast_config(&format!("http://{}", node.local_addr())).max_redirects(2))
            .expect("config");

    let err = client.info().unwrap_err();

    assert!(
        matches!(
            err,
            Error::Transport(belay_http::Error::TooManyRedirects { max: 2 })
        ),
        "{err}"
    );
    assert_eq!(
        hops.load(Ordering::SeqCst),
        3,
        "original request plus two hops"
    );
    node.shutdown_and_join();
}

#[test]
fn with_redirects_disabled_a_redirect_is_an_error() {
    let owner = Fake::start();
    let node = redirector(owner.url());
    let client =
        Client::new(fast_config(&format!("http://{}", node.local_addr())).max_redirects(0))
            .expect("config");
    let err = client.info().unwrap_err();
    assert!(err.api_error().is_some(), "{err}");
    assert_eq!(owner.count(), 0);
    node.shutdown_and_join();
}

// ---- long polls and timeouts --------------------------------------------

#[test]
fn a_long_poll_may_outlast_the_client_timeout() {
    let fake = Fake::start();
    fake.reply_with(|_| {
        thread::sleep(Duration::from_millis(250));
        Response::json_bytes(belay_proto::api::encode_body(&LeaseResponse::default()))
    });
    let client = fake.client_with(|c| c.timeout(Duration::from_millis(150)));
    let request = lease_request().wait(Duration::from_millis(200));

    let jobs = client
        .jobs()
        .lease(&request)
        .expect("answered within timeout + wait");

    assert!(jobs.is_empty());
    assert_eq!(fake.only().json()["wait"], json!("200ms"));
}

#[test]
fn a_hung_lease_times_out_without_a_second_attempt() {
    let fake = Fake::start();
    fake.reply_with(|_| {
        thread::sleep(Duration::from_millis(400));
        Response::json_bytes(belay_proto::api::encode_body(&LeaseResponse::default()))
    });
    let client = fake.client_with(|c| c.timeout(Duration::from_millis(100)));

    let err = client.jobs().lease(&lease_request()).unwrap_err();

    assert!(err.is_timeout(), "{err}");
    assert_eq!(fake.count(), 1);
}

#[test]
fn timed_out_reads_are_not_retried() {
    let fake = Fake::start();
    fake.reply_with(|_| {
        thread::sleep(Duration::from_millis(300));
        Response::json_bytes(belay_proto::api::encode_body(&sample_job(1)))
    });
    let client = fake.client_with(|c| c.timeout(Duration::from_millis(100)));

    let err = client.jobs().get(job_id(1)).unwrap_err();

    assert!(err.is_timeout(), "{err}");
    assert!(err.is_retryable(), "the caller may try again later");
    assert_eq!(fake.count(), 1, "timeout bounds the whole call");
}
