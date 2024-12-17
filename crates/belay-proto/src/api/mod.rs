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

/// Serializes a body as compact JSON.
#[must_use]
pub fn encode_body<T: ToJson + ?Sized>(value: &T) -> Vec<u8> {
    belay_json::to_string_of(value).into_bytes()
}

/// Reads an error response: the [`ApiError`] body if there is one,
/// otherwise a best guess from the status and raw body.
#[must_use]
pub fn decode_error(status: u16, body: &[u8]) -> ApiError {
    belay_json::from_bytes::<ApiError>(body)
        .unwrap_or_else(|_| ApiError::from_http_status(status, &String::from_utf8_lossy(body)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::JobSpec;

    #[test]
    fn decode_request_reports_parse_shape_and_rule_errors() {
        let err = decode_request::<EnqueueRequest>(b"{\"queue\": ").unwrap_err();
        assert!(err.message.starts_with("invalid JSON: "), "{}", err.message);
        let err = decode_request::<EnqueueRequest>(br#"{"queue": "q", "kind": 5}"#).unwrap_err();
        assert_eq!(err.message, "$.kind: expected string, found 5");
        let err = decode_request::<EnqueueRequest>(
            br#"{"jobs": [{"queue": "q", "kind": "k", "priority": 1001}]}"#,
        )
        .unwrap_err();
        assert_eq!(
            err.message,
            "$.jobs[0].priority: must be between -1000 and 1000, got 1001"
        );
        let ok = decode_request::<EnqueueRequest>(br#"{"queue": "q", "kind": "k"}"#).unwrap();
        assert_eq!(ok, EnqueueRequest::One(JobSpec::new("q", "k")));
    }

    #[test]
    fn error_bodies_decode_or_fall_back_to_the_status() {
        let err = ApiError::lease_lost("01HQ3K9Z8N6V2T4R5S7W0X1Y2Z".parse().unwrap());
        assert_eq!(decode_error(409, &encode_body(&err)), err);
        let fallback = decode_error(503, b"upstream down");
        assert_eq!(fallback.code, ErrorCode::Unavailable);
        assert_eq!(fallback.message, "HTTP 503: upstream down");
    }
}
