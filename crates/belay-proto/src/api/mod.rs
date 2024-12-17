//! Request and response bodies for the HTTP API (v1), and the helpers the
//! server and client use to read and write them.
//!
//! | endpoint | request | response |
//! |---|---|---|
//! | `GET /v1/info` | | [`ServerInfo`] |
//! | `POST /v1/jobs` | [`EnqueueRequest`] | [`EnqueueResponse`] |
//! | `GET /v1/jobs?...` | [`JobFilter`] (query) | [`Page`]`<`[`Job`](crate::Job)`>` |
//! | `GET /v1/jobs/:id`, `POST .../cancel`, `/retry`, `/promote` | | [`Job`](crate::Job) |
//! | `POST /v1/lease` | [`LeaseRequest`] | [`LeaseResponse`] |
//! | `POST /v1/jobs/:id/heartbeat` | [`HeartbeatRequest`] | [`HeartbeatResponse`] |
//! | `POST /v1/jobs/:id/complete` | [`CompleteRequest`] | empty (204) |
//! | `POST /v1/jobs/:id/fail` | [`FailRequest`] | empty (204) |
//! | `GET /v1/queues`, `GET /v1/queues/:name` | | [`Page`]`<`[`QueueInfo`](crate::QueueInfo)`>`, [`QueueInfo`](crate::QueueInfo) |
//! | `PUT /v1/queues/:name/config` | [`QueueConfig`](crate::QueueConfig) | [`QueueInfo`](crate::QueueInfo) |
//! | `POST /v1/queues/:name/purge?state=` | | [`PurgeResponse`] |
//! | `GET /v1/schedules`, `/v1/schedules/:id` | | [`Page`]`<`[`Schedule`](crate::Schedule)`>`, [`Schedule`](crate::Schedule) |
//! | `POST /v1/schedules`, `PUT /v1/schedules/:id` | [`ScheduleSpec`](crate::ScheduleSpec) | [`Schedule`](crate::Schedule) |
//! | `POST /v1/schedules/:id/trigger` | | [`TriggerResponse`] |
//! | `GET /v1/schedules/:id/preview?n=` | | [`PreviewResponse`] |
//! | `GET\|PUT /v1/calendars/:name` | [`CalendarSpec`](crate::CalendarSpec) | [`CalendarSpec`](crate::CalendarSpec) |
//! | `POST /v1/workflows` | [`WorkflowSpec`](crate::WorkflowSpec) | [`Workflow`](crate::Workflow) |
//! | `GET /v1/workflows[/:id]` | | [`Page`]`<`[`Workflow`](crate::Workflow)`>`, [`Workflow`](crate::Workflow) |
//! | `GET /v1/workers` | | [`Page`]`<`[`WorkerInfo`]`>` |
//! | `GET /v1/events?from=&types=&queue=` | [`EventFilter`](crate::event::EventFilter) (query) | SSE of [`Event`](crate::event::Event) |
//! | `GET /v1/cluster/members` | | [`Page`]`<`[`MemberInfo`]`>` |
//! | `POST /v1/admin/checkpoint` | | [`CheckpointResponse`] |
//!
//! Every failure is an [`ApiError`] body with the status from
//! [`ApiError::http_status`]. Lists always use [`Page`], unpaginated ones
//! with no `next_cursor`. Jobs returned by read endpoints are
//! [redacted](crate::Job::redacted); only lease responses carry tokens.

mod error;

mod filter;

mod info;

mod jobs;

pub use error::{ApiError, ErrorCode};
pub use filter::{Cursor, JobFilter, Page, SortOrder};
pub use info::{
    CheckpointResponse, MemberInfo, MemberState, PreviewResponse, PurgeResponse, ServerInfo,
    TriggerResponse, WorkerInfo,
};
pub use jobs::{
    CompleteRequest, EnqueueRequest, EnqueueResponse, EnqueueResult, EnqueueStatus, FailRequest,
    HeartbeatRequest, HeartbeatResponse, LeaseRequest, LeaseResponse, LeasedJob,
};

use belay_json::{FromJson, ToJson};

use crate::validate::Validate;

/// Reads a request body: strict JSON, then shape, then every rule. Any
/// failure is an `invalid_argument` [`ApiError`] ready to send back.
///
/// ```
/// use belay_proto::api::{decode_request, ErrorCode, LeaseRequest};
///
/// let req: LeaseRequest = decode_request(br#"{"worker": "w1", "queues": ["emails"]}"#).unwrap();
/// assert_eq!(req.max_jobs, 1);
///
/// let err = decode_request::<LeaseRequest>(br#"{"worker": "w1", "queues": []}"#).unwrap_err();
/// assert_eq!(err.code, ErrorCode::InvalidArgument);
/// assert_eq!(err.message, "$.queues: must name at least one queue");
/// ```
pub fn decode_request<T: FromJson + Validate>(body: &[u8]) -> Result<T, ApiError> {
    let value: T = belay_json::from_bytes(body)?;
    value.validate()?;
    Ok(value)
}
