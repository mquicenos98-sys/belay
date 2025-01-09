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

impl FromStr for JobState {
    type Err = ParseError;

    /// Accepts the lowercase names only; `"Ready"` is rejected with a hint.
    fn from_str(s: &str) -> Result<JobState, ParseError> {
        JobState::ALL
            .into_iter()
            .find(|state| state.as_str() == s)
            .ok_or_else(|| ParseError::unknown("job state", s, NAMES))
    }
}

text_json!(JobState, "job state");

/// A state change the state machine forbids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransitionError {
    /// The current state.
    pub from: JobState,
    /// The requested state.
    pub to: JobState,
}

impl fmt::Display for TransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.from == self.to {
            write!(f, "job is already {}", self.from)
        } else {
            write!(f, "cannot move job from {} to {}", self.from, self.to)
        }
    }
}

impl std::error::Error for TransitionError {}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_json::{json, FromJson, ToJson};

    #[test]
    fn every_state_is_reachable_from_ready() {
        let mut seen = vec![Ready];
        let mut frontier = vec![Ready];
        while let Some(s) = frontier.pop() {
            for &next in s.successors() {
                if !seen.contains(&next) {
                    seen.push(next);
                    frontier.push(next);
                }
            }
        }
        // Scheduled is an entry state only: nothing moves back into it.
        assert_eq!(seen.len(), 6);
        assert!(!seen.contains(&Scheduled));
    }

    #[test]
    fn terminal_states_only_leave_through_an_explicit_retry() {
        for s in JobState::ALL {
            if s.is_terminal() {
                assert!(s.successors().iter().all(|n| *n == Ready), "{s}");
            }
        }
        assert!(Succeeded.successors().is_empty());
    }

    #[test]
    fn classification_helpers() {
        let waiting: Vec<JobState> = JobState::ALL
            .into_iter()
            .filter(|s| s.is_waiting())
            .collect();
        assert_eq!(waiting, [Scheduled, Retrying]);
        let terminal: Vec<JobState> = JobState::ALL
            .into_iter()
            .filter(|s| s.is_terminal())
            .collect();
        assert_eq!(terminal, [Succeeded, Dead, Cancelled]);
        assert!(Running.is_pending() && !Dead.is_pending());
    }

    #[test]
    fn names_round_trip_and_reject_other_cases() {
        for s in JobState::ALL {
            assert_eq!(s.to_string().parse::<JobState>(), Ok(s));
            assert_eq!(JobState::from_json(&s.to_json()), Ok(s));
        }
        assert_eq!(
            "Ready".parse::<JobState>().unwrap_err().to_string(),
            r#"unknown job state "Ready", did you mean "ready"?"#
        );
        assert_eq!(
            JobState::from_json(&json!("canceled"))
                .unwrap_err()
                .to_string(),
            r#"$: unknown job state "canceled", did you mean "cancelled"?"#
        );
        assert_eq!(format!("[{:<9}]", Ready), "[ready    ]");
    }

    #[test]
    fn transition_errors_read_naturally() {
        assert_eq!(
            Succeeded.check_transition(Running).unwrap_err().to_string(),
            "cannot move job from succeeded to running"
        );
        assert_eq!(
            Running.check_transition(Running).unwrap_err().to_string(),
            "job is already running"
        );
    }
}
