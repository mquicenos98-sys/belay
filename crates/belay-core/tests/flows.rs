//! End-to-end flows through one engine on a manual clock: jobs, retries,
//! schedules, workflows, the event stream and metrics.

mod common;

use std::time::Duration;

use belay_common::BackoffPolicy;
use belay_core::{Config, Delivery, Paging};
use belay_json::json;
use belay_proto::api::{ErrorCode, HeartbeatRequest, JobFilter};
use belay_proto::event::{EventFilter, EventKind, EventPattern};
use belay_proto::workflow::{StepSpec, StepState, WorkflowState};
use belay_proto::{
    JobSource, JobSpec, JobState, QueueConfig, QueueName, ScheduleSpec, WorkflowSpec,
};
use common::{everything, Harness};

const MINUTE: Duration = Duration::from_secs(60);

const HOUR: Duration = Duration::from_secs(3600);

#[test]
fn enqueue_lease_complete() {
    let h = Harness::new();
    let first_seq = h.engine.events().next_seq();
    let id = h
        .engine
        .enqueue(JobSpec::new("emails", "send_welcome").payload(json!({"user": 42})))
        .unwrap()
        .id;
    assert_eq!(h.engine.get_job(&id).unwrap().state, JobState::Ready);

    let leased = h.lease_one("emails");
    assert_eq!(leased.job.id, id);
    assert_eq!(leased.job.spec.payload["user"], 42);
    let beat = h
        .engine
        .heartbeat(&id, &HeartbeatRequest::new(leased.token))
        .unwrap();
    assert!(!beat.cancel_requested);

    h.clock.advance(Duration::from_secs(2));
    h.complete(&leased, Some(json!({"sent": true})));
    let job = h.engine.get_job(&id).unwrap();
    assert_eq!(job.state, JobState::Succeeded);
    assert_eq!(job.result, Some(json!({"sent": true})));
    assert_eq!(
        h.event_types(first_seq),
        [
            "job.enqueued",
            "worker.seen",
            "job.started",
            "job.succeeded"
        ]
    );

    // The same token cannot report twice.
    let again = h
        .engine
        .complete(&id, &belay_proto::api::CompleteRequest::new(leased.token));
    assert_eq!(again.unwrap_err().code, ErrorCode::LeaseLost);
}
