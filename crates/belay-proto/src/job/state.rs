//! The job state machine.
//!
//! ```text
//! scheduled -> ready      (run_at reached, or promoted)
//!           -> cancelled
//! ready     -> running    (leased by a worker)
//!           -> cancelled
//! running   -> succeeded  (completed)
//!           -> retrying   (failed with attempts left)
//!           -> dead       (failed for good: exhausted or non-retryable)
//!           -> cancelled  (cancellation requested, then acknowledged)
//! retrying  -> ready      (backoff elapsed, or promoted)
//!           -> running    (leased once the backoff elapsed)
//!           -> cancelled
//! succeeded -> (nothing)
//! dead      -> ready      (explicit retry)
//! cancelled -> ready      (explicit retry)
//! ```
//!
//! [`JobState::successors`] is the authoritative table. Lease expiry is a
//! failure like any other (`running` to `retrying` or `dead`). Nothing moves
//! back into `scheduled`: a job enters there or in `ready`.

use std::fmt;
use std::str::FromStr;

use crate::error::ParseError;
use crate::wire::text_json;

/// Where a job is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum JobState {
    /// Waiting for its `run_at` time.
    Scheduled,
    /// Eligible to be leased.
    Ready,
    /// Leased by a worker.
    Running,
    /// An attempt failed; waiting out the backoff delay before the next.
    Retrying,
    /// Finished successfully.
    Succeeded,
    /// Failed for good: attempts exhausted or a non-retryable error.
    Dead,
    /// Cancelled before it could finish.
    Cancelled,
}

use JobState::*;

const NAMES: &[&str] = &[
    "scheduled",
    "ready",
    "running",
    "retrying",
    "succeeded",
    "dead",
    "cancelled",
];
