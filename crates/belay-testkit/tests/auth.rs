//! Bearer tokens and scopes end to end: each token may do what its scope
//! allows and nothing more, workers need `work`, the event stream needs
//! `read`, refusals carry the RFC 6750 challenge, a token over its request
//! rate is told when to come back, and the access log names the token
//! behind each request.

use belay_core::config::Scope;
use belay_proto::api::LeaseRequest;
use belay_proto::WorkerId;
use belay_server::RequestRate;
use belay_testkit::prelude::*;
use belay_testkit::{raw, tokens};

fn code<T: std::fmt::Debug>(
    result: std::result::Result<T, belay_client::Error>,
) -> Option<ErrorCode> {
    result.expect_err("the call should be refused").code()
}

#[test]
fn each_token_does_what_its_scope_allows_and_nothing_more() -> Result<()> {
    let server = TestServer::builder().standard_tokens().start()?;
    let anonymous = server.client();
    let impostor = server.client_with_token("not-a-token-anyone-issued")?;
    let reader = server.client_with_token(tokens::READER)?;
    let producer = server.client_with_token(tokens::PRODUCER)?;
    let admin = server.client_with_token(tokens::ADMIN)?;
    let spec = job("emails", "send");
    let emails = QueueName::from_static("emails");

    assert_eq!(
        code(anonymous.jobs().enqueue(&spec)),
        Some(ErrorCode::Unauthenticated)
    );
    assert_eq!(
        code(impostor.jobs().enqueue(&spec)),
        Some(ErrorCode::Unauthenticated)
    );
    assert_eq!(
        code(reader.jobs().enqueue(&spec)),
        Some(ErrorCode::PermissionDenied)
    );

    let id = producer.jobs().enqueue(&spec)?.id;
    assert_eq!(reader.jobs().get(id)?.state, JobState::Ready);
    // Producers may withdraw their own work but not look around or operate.
    assert_eq!(
        code(producer.jobs().get(id)),
        Some(ErrorCode::PermissionDenied)
    );
    let lease = LeaseRequest::new(WorkerId::from_static("sneaky"), vec![emails.clone()]);
    assert_eq!(
        code(producer.jobs().lease(&lease)),
        Some(ErrorCode::PermissionDenied)
    );
    assert_eq!(
        code(reader.jobs().lease(&lease)),
        Some(ErrorCode::PermissionDenied)
    );
    assert_eq!(
        code(reader.queues().pause(&emails)),
        Some(ErrorCode::PermissionDenied)
    );
    assert_eq!(
        code(producer.queues().pause(&emails)),
        Some(ErrorCode::PermissionDenied)
    );

    // A worker with the work token leases and reports.
    let worker = server
        .worker()
        .token(tokens::WORKER)
        .queue("emails")
        .recording("send")
        .start()?;
    let done = wait_for_state(&reader, id, JobState::Succeeded, DEFAULT_TIMEOUT)?;
    assert_eq!(done.attempt, 1);
    assert_eq!(worker.recorder().len(), 1);

    // Retrying is an operator's call: admin only.
    let dead = producer
        .jobs()
        .enqueue(&job("emails", "missing-handler").max_attempts(1))?
        .id;
    wait_for_state(&reader, dead, JobState::Dead, DEFAULT_TIMEOUT)?;
    assert_eq!(
        code(producer.jobs().retry(dead)),
        Some(ErrorCode::PermissionDenied)
    );
    assert_eq!(admin.jobs().retry(dead)?.state, JobState::Ready);
    admin.queues().pause(&emails)?;
    assert!(reader.queues().get(&emails)?.paused);
    Ok(())
}

#[test]
fn a_worker_without_the_work_scope_is_turned_away_on_every_lease() -> Result<()> {
    let server = TestServer::builder().standard_tokens().start()?;
    let producer = server.client_with_token(tokens::PRODUCER)?;
    let reader = server.client_with_token(tokens::READER)?;
    let id = producer.jobs().enqueue(&job("emails", "send"))?.id;
    let worker = server
        .worker()
        .token(tokens::READER)
        .queue("emails")
        .recording("send")
        .start()?;
    worker.wait_for_failed_calls(3, DEFAULT_TIMEOUT)?;
    assert!(worker.recorder().is_empty());
    assert!(worker.stats().lease_errors >= 3);
    let last = worker.stats().last_error.unwrap_or_default();
    assert!(last.contains("lacks the work scope"), "{last}");
    assert_eq!(reader.jobs().get(id)?.state, JobState::Ready);
    Ok(())
}

#[test]
fn the_event_stream_needs_the_read_scope() -> Result<()> {
    let server = TestServer::builder().standard_tokens().start()?;
    let producer = server.client_with_token(tokens::PRODUCER)?;
    let refused = EventLog::start(&producer, &EventFilter::new()).expect_err("no read scope");
    assert_eq!(
        refused.api_error().map(|e| e.code),
        Some(ErrorCode::PermissionDenied)
    );
    let reader = server.client_with_token(tokens::READER)?;
    let events = EventLog::start(&reader, &EventFilter::new())?;
    let id = producer.jobs().enqueue(&job("emails", "send"))?.id;
    events.wait_for_job_event(id, "job.enqueued", DEFAULT_TIMEOUT)?;
    Ok(())
}

#[test]
fn refusals_carry_a_bearer_challenge_and_probes_need_no_token() -> Result<()> {
    let server = TestServer::builder().standard_tokens().start()?;
    let jobs = format!("{}/v1/jobs", server.url());

    let missing = raw::request("GET", &jobs, &[], b"")?;
    assert_eq!(missing.status, 401);
    let challenge = missing.header("www-authenticate").unwrap_or_default();
    assert!(challenge.starts_with("Bearer"), "{challenge:?}");
    assert_eq!(missing.json()?["error"]["code"], "unauthenticated");

    let bearer = format!("Bearer {}", tokens::PRODUCER);
    let denied = raw::request("GET", &jobs, &[("Authorization", &bearer)], b"")?;
    assert_eq!(denied.status, 403);
    assert_eq!(denied.json()?["error"]["code"], "permission_denied");

    for probe in ["/healthz", "/readyz"] {
        let answer = raw::request("GET", &format!("{}{probe}", server.url()), &[], b"")?;
        assert_eq!(answer.status, 200, "{probe}");
    }
    let metrics = raw::request("GET", &format!("{}/metrics", server.url()), &[], b"")?;
    assert_eq!(metrics.status, 401);
    let reader = server.client_with_token(tokens::READER)?;
    assert!(reader.metrics()?.contains("belay_http_requests_total"));
    Ok(())
}

#[test]
fn the_access_log_names_the_token_behind_each_request() -> Result<()> {
    let capture = LogCapture::start();
    let server = TestServer::builder()
        .token("auditor", "auditor-token-0123456789", &[Scope::Read])
        .token("loader", "loader-token-0123456789", &[Scope::Enqueue])
        .start()?;
    let auditor = server.client_with_token("auditor-token-0123456789")?;
    let loader = server.client_with_token("loader-token-0123456789")?;
    let target = belay_server::ACCESS_LOG_TARGET;

    let id = loader.jobs().enqueue(&job("audit", "check"))?.id;
    let created = capture.wait_for(DEFAULT_TIMEOUT, |r| {
        r.target == target && r.field("principal") == Some("loader")
    })?;
    assert_eq!(created.field("route"), Some("/v1/jobs"));
    assert_eq!(created.field("status"), Some("201"));

    auditor.jobs().get(id)?;
    let read = capture.wait_for(DEFAULT_TIMEOUT, |r| {
        r.target == target && r.field("principal") == Some("auditor")
    })?;
    assert_eq!(read.field("route"), Some("/v1/jobs/:id"));
    assert_eq!(read.message, format!("GET /v1/jobs/{id} 200"));

    // A refusal is found by the request id the server hands back.
    let refused = raw::request(
        "POST",
        &format!("{}/v1/jobs", server.url()),
        &[("Authorization", "Bearer auditor-token-0123456789")],
        br#"{"queue": "audit", "kind": "check"}"#,
    )?;
    assert_eq!(refused.status, 403);
    let request_id = refused
        .header("x-request-id")
        .expect("every response carries its request id")
        .to_owned();
    let line = capture.wait_for(DEFAULT_TIMEOUT, |r| {
        r.target == target && r.field("request_id") == Some(request_id.as_str())
    })?;
    assert_eq!(line.field("status"), Some("403"));
    assert_eq!(line.field("route"), Some("/v1/jobs"));
    Ok(())
}

#[test]
fn a_token_over_its_request_rate_is_told_when_to_come_back() -> Result<()> {
    const BURSTY: &str = "bursty-token-0123456789";
    const CALM: &str = "calm-token-0123456789";
    // One request every ten seconds after a burst of two: the third
    // request in a row is always over.
    let server = TestServer::builder()
        .token("bursty", BURSTY, &[Scope::Read])
        .token("calm", CALM, &[Scope::Read])
        .configure_options(|options| {
            options
                .token_rate_limits
                .insert("bursty".to_owned(), Some(RequestRate::new(0.1, 2)));
        })
        .start()?;
    let queues = format!("{}/v1/queues", server.url());
    let bursty = format!("Bearer {BURSTY}");
    let statuses: Vec<u16> = (0..3)
        .map(|_| Ok(raw::request("GET", &queues, &[("Authorization", &bursty)], b"")?.status))
        .collect::<Result<_>>()?;
    assert_eq!(statuses, [200, 200, 429]);

    let limited = raw::request("GET", &queues, &[("Authorization", &bursty)], b"")?;
    assert_eq!(limited.status, 429);
    assert_eq!(limited.json()?["error"]["code"], "resource_exhausted");
    let retry_after: u64 = limited
        .header("retry-after")
        .and_then(|v| v.parse().ok())
        .expect("a 429 says when to retry");
    assert!((1..=10).contains(&retry_after), "{retry_after}");

    // Limits are per token.
    let calm = raw::request(
        "GET",
        &queues,
        &[("Authorization", &format!("Bearer {CALM}"))],
        b"",
    )?;
    assert_eq!(calm.status, 200);
    Ok(())
}
