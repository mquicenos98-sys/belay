//! `/v1/jobs` and `/v1/lease`: enqueueing, inspection, management, and the
//! worker protocol (lease, heartbeat, complete, fail), which also backs the
//! client's [`JobSource`] implementation.

use belay_json::{ObjectBuilder, Value};
use belay_proto::api::{
    ApiError, CompleteRequest, EnqueueResponse, EnqueueResult, FailRequest, HeartbeatRequest,
    HeartbeatResponse, JobFilter, LeaseRequest, LeaseResponse, LeasedJob, Page,
};
use belay_proto::{Job, JobId, JobSource, JobSpec};

use crate::client::{segment, Call, Client};
use crate::error::{DecodeError, Result};
use crate::pager::Paginator;
use crate::retry::{is_repeatable_enqueue, Safety};

/// Job calls; see [`Client::jobs`].
#[derive(Clone, Copy, Debug)]
pub struct Jobs<'a> {
    client: &'a Client,
}

fn job_path(id: JobId, action: &str) -> String {
    let id = segment(&id.to_string());
    if action.is_empty() {
        format!("/v1/jobs/{id}")
    } else {
        format!("/v1/jobs/{id}/{action}")
    }
}
