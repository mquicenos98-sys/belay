//! [`MemorySource`]: an in-memory [`JobSource`] for testing handlers and
//! workers without a server, with injectable failures and latency.
//!
//! It follows the lease protocol (tokens, attempt counting, `lease_lost`
//! for stale or revoked leases, long polling) but is not a queue engine:
//! jobs are ready as soon as they are pushed (`run_at` and `delay` are
//! ignored), retry delays are ignored, and nothing expires on its own —
//! use [`MemorySource::revoke_lease`] to simulate an expired lease.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use belay_common::rand::Rng;
use belay_common::sync::{lock, wait_timeout};
use belay_common::Timestamp;
use belay_json::Value;
use belay_proto::api::{
    ApiError, CompleteRequest, FailRequest, HeartbeatRequest, HeartbeatResponse, LeaseRequest,
    LeasedJob,
};
use belay_proto::source::JobSource;
use belay_proto::{Job, JobId, JobSpec, JobState, Lease, LeaseToken, Progress};

/// How an attempt ended, as reported to a [`MemorySource`].
#[derive(Clone, Debug, PartialEq)]
pub enum JobOutcome {
    /// `complete` with this result.
    Completed(Option<Value>),
    /// `fail` with this request.
    Failed(FailRequest),
}

impl JobOutcome {
    /// The failure, if this is one.
    #[must_use]
    pub fn failure(&self) -> Option<&FailRequest> {
        match self {
            JobOutcome::Failed(req) => Some(req),
            JobOutcome::Completed(_) => None,
        }
    }

    /// The result, if this is a completion (`Some(None)` for a completion
    /// without a result).
    #[must_use]
    pub fn result(&self) -> Option<Option<&Value>> {
        match self {
            JobOutcome::Completed(result) => Some(result.as_ref()),
            JobOutcome::Failed(_) => None,
        }
    }
}
