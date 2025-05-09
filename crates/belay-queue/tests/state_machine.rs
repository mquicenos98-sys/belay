//! The job state machine as the engine implements it: every operation in
//! every state, then the finer points of each transition.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use belay_common::BackoffPolicy;
use belay_json::json;
use belay_proto::api::{CompleteRequest, ErrorCode, FailRequest, HeartbeatRequest};
use belay_proto::{JobId, JobSpec, JobState, LeaseToken, Progress};
use belay_queue::Error;
use support::*;

const ALL: [JobState; 7] = JobState::ALL;

/// Operations applied to a job in the exhaustive table.
#[derive(Clone, Copy, Debug)]
enum Op {
    Lease,
    Heartbeat,
    Complete,
    FailRetryable,
    FailFatal,
    Cancel,
    Retry,
    Promote,
    Delete,
    /// A minute passes and the engine ticks.
    Tick,
}

const OPS: [Op; 10] = [
    Op::Lease,
    Op::Heartbeat,
    Op::Complete,
    Op::FailRetryable,
    Op::FailFatal,
    Op::Cancel,
    Op::Retry,
    Op::Promote,
    Op::Delete,
    Op::Tick,
];

#[derive(Debug, PartialEq)]
enum Want {
    /// The operation succeeds and the job ends up in this state.
    Now(JobState),
    /// The job no longer exists.
    Gone,
    /// The operation fails with this code and the job is unchanged.
    Fails(ErrorCode),
}
