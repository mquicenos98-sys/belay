//! Queues, schedules, calendars, workflows, the server-wide calls and the
//! admin endpoints: request shapes and response decoding.

mod common;

use std::time::Duration;

use belay_client::ListOptions;
use belay_http::{Response, StatusCode};
use belay_json::{json, ToJson};
use belay_proto::api::{
    ApiError, CheckpointResponse, Cursor, ErrorCode, MemberInfo, MemberState, Page,
    PreviewResponse, PurgeResponse, ServerInfo, TriggerResponse, WorkerInfo,
};
use belay_proto::workflow::StepSpec;
use belay_proto::{
    CalendarSpec, JobSpec, JobState, MisfirePolicy, QueueConfig, QueueInfo, QueueName, RateLimit,
    Schedule, ScheduleId, ScheduleSpec, WorkerId, Workflow, WorkflowId, WorkflowSpec,
};
use common::{job_id, t0, Fake};

fn queue(name: &'static str) -> QueueName {
    QueueName::from_static(name)
}

fn schedule_spec() -> ScheduleSpec {
    ScheduleSpec::new(
        ScheduleId::from_static("nightly"),
        "0 2 * * *",
        JobSpec::new("reports", "build"),
    )
    .timezone("America/Bogota")
    .misfire(MisfirePolicy::FireOnce)
}

fn schedule() -> Schedule {
    Schedule::new(schedule_spec(), Some(t0()), t0())
}
