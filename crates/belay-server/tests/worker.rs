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
