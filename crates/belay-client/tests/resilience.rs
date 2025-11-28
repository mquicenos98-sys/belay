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
