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

impl JobState {
    /// Every state, in lifecycle order.
    pub const ALL: [JobState; 7] = [
        Scheduled, Ready, Running, Retrying, Succeeded, Dead, Cancelled,
    ];

    /// The lowercase name used in JSON, query strings and the CLI.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Scheduled => "scheduled",
            Ready => "ready",
            Running => "running",
            Retrying => "retrying",
            Succeeded => "succeeded",
            Dead => "dead",
            Cancelled => "cancelled",
        }
    }

    /// Whether the job is finished: nothing will happen to it without an
    /// explicit retry. Terminal jobs are subject to retention and may be
    /// deleted.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Succeeded | Dead | Cancelled)
    }

    /// Whether the job is waiting on a timer (`scheduled` or `retrying`).
    #[must_use]
    pub const fn is_waiting(self) -> bool {
        matches!(self, Scheduled | Retrying)
    }

    /// Whether the job still has work ahead of it: not terminal.
    #[must_use]
    pub const fn is_pending(self) -> bool {
        !self.is_terminal()
    }

    /// The states this one may move to.
    #[must_use]
    pub const fn successors(self) -> &'static [JobState] {
        match self {
            Scheduled => &[Ready, Cancelled],
            Ready => &[Running, Cancelled],
            Running => &[Succeeded, Retrying, Dead, Cancelled],
            Retrying => &[Ready, Running, Cancelled],
            Succeeded => &[],
            Dead | Cancelled => &[Ready],
        }
    }

    /// Whether moving from `self` to `next` is allowed. Staying in the same
    /// state is not a transition and is never allowed.
    #[must_use]
    pub fn can_transition_to(self, next: JobState) -> bool {
        self.successors().contains(&next)
    }

    /// Like [`can_transition_to`](Self::can_transition_to) with an error
    /// suitable for returning to a caller.
    pub fn check_transition(self, next: JobState) -> Result<(), TransitionError> {
        if self.can_transition_to(next) {
            Ok(())
        } else {
            Err(TransitionError {
                from: self,
                to: next,
            })
        }
    }
}

impl fmt::Display for JobState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}
