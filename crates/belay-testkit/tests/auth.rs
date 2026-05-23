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
