//! The stored job record ([`Job`]) and what hangs off it: the current
//! [`Lease`], the error history ([`JobError`]) and reported [`Progress`].
//!
//! Records are produced by the server, so decoding them ignores unknown
//! fields: an older client can read a newer server's jobs.

use std::time::Duration;

use belay_common::{BackoffPolicy, Rng, Timestamp};
use belay_json::{FromJson, FromJsonError, ObjectBuilder, ObjectReader, ToJson, Value};

use crate::ids::{JobId, LeaseToken, QueueName, WorkerId, LABEL_RULE};
use crate::job::{JobSpec, JobState, TransitionError};
use crate::limits::{
    DEFAULT_MAX_ATTEMPTS, MAX_ERROR_HISTORY, MAX_ERROR_KIND_LEN, MAX_ERROR_MESSAGE_BYTES,
    MAX_PROGRESS_MESSAGE_BYTES,
};
use crate::validate::{Validate, Validator};
use crate::wire::{BuilderExt, ReaderExt};

/// A job and everything that has happened to it.
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    /// Its id; also encodes when it was created.
    pub id: JobId,
    /// What was submitted, with queue defaults filled in.
    pub spec: JobSpec,
    /// Where it is in its life.
    pub state: JobState,
    /// Attempts started so far: 0 until first leased, 1 while the first
    /// attempt runs.
    pub attempt: u32,
    /// When it was enqueued.
    pub created_at: Timestamp,
    /// When anything about it last changed.
    pub updated_at: Timestamp,
    /// When it becomes (or became) eligible to run: the spec's run time at
    /// first, then the end of each backoff delay.
    pub run_at: Timestamp,
    /// When the latest attempt started.
    pub started_at: Option<Timestamp>,
    /// When it reached a terminal state.
    pub finished_at: Option<Timestamp>,
    /// The current lease, while running.
    pub lease: Option<Lease>,
    /// Someone asked to cancel it while it was running; the worker learns
    /// this from its next heartbeat.
    pub cancel_requested: bool,
    /// Failed attempts, oldest first, at most
    /// [`MAX_ERROR_HISTORY`](crate::limits::MAX_ERROR_HISTORY).
    pub errors: Vec<JobError>,
    /// What the handler returned, once succeeded.
    pub result: Option<Value>,
    /// Latest progress report of the running attempt.
    pub progress: Option<Progress>,
}

impl Job {
    /// A freshly enqueued job: `scheduled` if its run time is in the future,
    /// `ready` otherwise.
    #[must_use]
    pub fn new(id: JobId, spec: JobSpec, now: Timestamp) -> Job {
        let run_at = spec.run_time(now);
        Job {
            id,
            state: if run_at > now {
                JobState::Scheduled
            } else {
                JobState::Ready
            },
            spec,
            attempt: 0,
            created_at: now,
            updated_at: now,
            run_at,
            started_at: None,
            finished_at: None,
            lease: None,
            cancel_requested: false,
            errors: Vec::new(),
            result: None,
            progress: None,
        }
    }

    /// The job's queue.
    #[must_use]
    pub fn queue(&self) -> &QueueName {
        &self.spec.queue
    }

    /// The handler name.
    #[must_use]
    pub fn kind(&self) -> &str {
        &self.spec.kind
    }

    /// See [`JobState::is_terminal`].
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }

    /// Whether the job is running on a lease that has run out. A running job
    /// without a lease (a damaged record) also counts as expired, so that
    /// the reaper recovers it instead of leaving it stuck.
    #[must_use]
    pub fn lease_expired(&self, now: Timestamp) -> bool {
        self.state == JobState::Running && self.lease.as_ref().map_or(true, |l| l.is_expired(now))
    }

    /// Time since enqueue.
    #[must_use]
    pub fn age(&self, now: Timestamp) -> Duration {
        now.saturating_duration_since(self.created_at)
    }

    /// How long the latest attempt ran, once finished.
    #[must_use]
    pub fn runtime(&self) -> Option<Duration> {
        Some(
            self.finished_at?
                .saturating_duration_since(self.started_at?),
        )
    }

    /// Total attempts allowed.
    #[must_use]
    pub fn max_attempts(&self) -> u32 {
        self.spec.max_attempts.unwrap_or(DEFAULT_MAX_ATTEMPTS)
    }

    /// Attempts not yet started.
    #[must_use]
    pub fn attempts_left(&self) -> u32 {
        self.max_attempts().saturating_sub(self.attempt)
    }

    /// Where a failure of the running attempt takes the job: `cancelled` if
    /// cancellation was requested (the worker stopped because it was asked
    /// to), `retrying` if the error is retryable and attempts remain, `dead`
    /// otherwise.
    #[must_use]
    pub fn state_after_failure(&self, retryable: bool) -> JobState {
        if self.cancel_requested {
            JobState::Cancelled
        } else if retryable && self.attempts_left() > 0 {
            JobState::Retrying
        } else {
            JobState::Dead
        }
    }

    /// The backoff delay after the attempt that just failed, from the job's
    /// own policy or `fallback` (the queue's). Randomness comes from the
    /// caller so that engines can seed it in tests.
    pub fn retry_delay(&self, fallback: &BackoffPolicy, rng: &mut Rng) -> Duration {
        self.spec
            .retry
            .as_ref()
            .unwrap_or(fallback)
            .delay_for(self.attempt.max(1), rng)
    }

    /// When the running attempt's execution budget runs out, if it has one.
    #[must_use]
    pub fn deadline(&self) -> Option<Timestamp> {
        Some(self.started_at? + self.spec.timeout?)
    }

    /// Moves to `next`, keeping the bookkeeping fields consistent:
    /// `updated_at` is set; entering a terminal state sets `finished_at`,
    /// leaving one clears it; leaving `running` drops the lease, progress
    /// and any cancellation request.
    ///
    /// Attempt counting and lease creation belong to the caller, which knows
    /// the worker and token.
    pub fn transition(&mut self, next: JobState, now: Timestamp) -> Result<(), TransitionError> {
        self.state.check_transition(next)?;
        if self.state == JobState::Running {
            self.lease = None;
            self.progress = None;
            self.cancel_requested = false;
        }
        self.finished_at = if next.is_terminal() { Some(now) } else { None };
        self.state = next;
        self.updated_at = now;
        Ok(())
    }

    /// Appends to the error history, dropping the oldest entries beyond
    /// [`MAX_ERROR_HISTORY`](crate::limits::MAX_ERROR_HISTORY).
    pub fn record_error(&mut self, error: JobError) {
        self.errors.push(error);
        self.errors.truncate(MAX_ERROR_HISTORY);
    }

    /// The most recent failure.
    #[must_use]
    pub fn last_error(&self) -> Option<&JobError> {
        self.errors.last()
    }

    /// A copy safe to show to clients that do not hold the lease: the lease
    /// token is replaced by [`LeaseToken::REDACTED`], which JSON omits.
    #[must_use]
    pub fn redacted(&self) -> Job {
        let mut job = self.clone();
        if let Some(lease) = &mut job.lease {
            lease.token = LeaseToken::REDACTED;
        }
        job
    }
}

impl ToJson for Job {
    fn to_json(&self) -> Value {
        ObjectBuilder::with_capacity(14)
            .field("id", self.id)
            .field("state", self.state)
            .field("attempt", self.attempt)
            .field("spec", &self.spec)
            .timestamp("created_at", self.created_at)
            .timestamp("updated_at", self.updated_at)
            .timestamp("run_at", self.run_at)
            .opt_timestamp("started_at", self.started_at)
            .opt_timestamp("finished_at", self.finished_at)
            .opt_field("lease", &self.lease)
            .field_if(self.cancel_requested, "cancel_requested", true)
            .field_if(!self.errors.is_empty(), "errors", &self.errors)
            .opt_field("result", &self.result)
            .opt_field("progress", &self.progress)
            .build()
    }
}
