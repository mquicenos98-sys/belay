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
        if self.errors.len() > MAX_ERROR_HISTORY {
            let excess = self.errors.len() - MAX_ERROR_HISTORY;
            self.errors.drain(..excess);
        }
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

impl FromJson for Job {
    fn from_json(value: &Value) -> Result<Job, FromJsonError> {
        let r = ObjectReader::new(value, "Job")?;
        Ok(Job {
            id: r.req("id")?,
            state: r.req("state")?,
            attempt: r.or("attempt", 0)?,
            spec: r.req("spec")?,
            created_at: r.req_timestamp("created_at")?,
            updated_at: r.req_timestamp("updated_at")?,
            run_at: r.req_timestamp("run_at")?,
            started_at: r.opt_timestamp("started_at")?,
            finished_at: r.opt_timestamp("finished_at")?,
            lease: r.opt("lease")?,
            cancel_requested: r.or_default("cancel_requested")?,
            errors: r.or_default("errors")?,
            // `result: null` is a real result (the handler returned null)
            // and must survive a round trip, so read the raw value.
            result: r.value("result").cloned(),
            progress: r.opt("progress")?,
        })
    }
}

/// One failed attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobError {
    /// When the failure was recorded.
    pub at: Timestamp,
    /// Which attempt failed (1-based).
    pub attempt: u32,
    /// What went wrong, truncated to
    /// [`MAX_ERROR_MESSAGE_BYTES`](crate::limits::MAX_ERROR_MESSAGE_BYTES).
    pub message: String,
    /// A machine-readable class such as `timeout` or `http_503`.
    pub kind: Option<String>,
    /// Whether the handler considered it worth retrying.
    pub retryable: bool,
    /// The worker that reported it, if any (lease expiry has none).
    pub worker: Option<WorkerId>,
}

impl JobError {
    /// A retryable error, truncating `message` on a character boundary.
    #[must_use]
    pub fn new(at: Timestamp, attempt: u32, message: impl Into<String>) -> JobError {
        JobError {
            at,
            attempt,
            message: truncate(message.into(), MAX_ERROR_MESSAGE_BYTES),
            kind: None,
            retryable: true,
            worker: None,
        }
    }

    /// Sets the kind.
    #[must_use]
    pub fn kind(mut self, kind: impl Into<String>) -> JobError {
        self.kind = Some(kind.into());
        self
    }

    /// Sets whether the error is retryable.
    #[must_use]
    pub fn retryable(mut self, retryable: bool) -> JobError {
        self.retryable = retryable;
        self
    }

    /// Sets the reporting worker.
    #[must_use]
    pub fn worker(mut self, worker: WorkerId) -> JobError {
        self.worker = Some(worker);
        self
    }
}

/// Cuts `s` to at most `max` bytes without splitting a character, marking
/// the cut with an ellipsis.
fn truncate(mut s: String, max: usize) -> String {
    const MARK: &str = "...";
    if s.len() <= max {
        return s;
    }
    let mut cut = max.saturating_sub(MARK.len());
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s.push_str(MARK);
    s
}

impl ToJson for JobError {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .timestamp("at", self.at)
            .field("attempt", self.attempt)
            .field("message", &self.message)
            .opt_field("kind", &self.kind)
            .field("retryable", self.retryable)
            .opt_field("worker", &self.worker)
            .build()
    }
}

impl FromJson for JobError {
    fn from_json(value: &Value) -> Result<JobError, FromJsonError> {
        let r = ObjectReader::new(value, "JobError")?;
        Ok(JobError {
            at: r.req_timestamp("at")?,
            attempt: r.req("attempt")?,
            message: r.req("message")?,
            kind: r.opt("kind")?,
            retryable: r.or("retryable", true)?,
            worker: r.opt("worker")?,
        })
    }
}

/// A worker's claim on a running job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    /// Who holds it.
    pub worker: WorkerId,
    /// Proof of holding it.
    pub token: LeaseToken,
    /// When it was granted.
    pub acquired_at: Timestamp,
    /// When it lapses unless renewed.
    pub expires_at: Timestamp,
    /// When it was last renewed (initially `acquired_at`).
    pub heartbeat_at: Timestamp,
}

impl Lease {
    /// A lease granted at `now` for `duration`.
    #[must_use]
    pub fn new(worker: WorkerId, token: LeaseToken, now: Timestamp, duration: Duration) -> Lease {
        Lease {
            worker,
            token,
            acquired_at: now,
            expires_at: now + duration,
            heartbeat_at: now,
        }
    }

    /// Whether it has lapsed at `now`. A lease is valid strictly before its
    /// expiry instant.
    #[must_use]
    pub fn is_expired(&self, now: Timestamp) -> bool {
        now >= self.expires_at
    }

    /// Time left before it lapses.
    #[must_use]
    pub fn remaining(&self, now: Timestamp) -> Duration {
        self.expires_at.saturating_duration_since(now)
    }

    /// Renews at `now` so that it lapses `by` from now, but never earlier
    /// than it already would (a short heartbeat does not shorten a lease).
    pub fn renew(&mut self, now: Timestamp, by: Duration) {
        self.heartbeat_at = now;
        self.expires_at = self.expires_at.max(now + by);
    }
}

impl ToJson for Lease {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("worker", &self.worker)
            .field_if(!self.token.is_redacted(), "token", self.token)
            .timestamp("acquired_at", self.acquired_at)
            .timestamp("expires_at", self.expires_at)
            .timestamp("heartbeat_at", self.heartbeat_at)
            .build()
    }
}

impl FromJson for Lease {
    fn from_json(value: &Value) -> Result<Lease, FromJsonError> {
        let r = ObjectReader::new(value, "Lease")?;
        let acquired_at = r.req_timestamp("acquired_at")?;
        Ok(Lease {
            worker: r.req("worker")?,
            token: r.or("token", LeaseToken::REDACTED)?,
            acquired_at,
            expires_at: r.req_timestamp("expires_at")?,
            heartbeat_at: r.opt_timestamp("heartbeat_at")?.unwrap_or(acquired_at),
        })
    }
}

/// How far a running attempt has got, as reported by its handler.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Progress {
    /// Completion in percent, `0..=100`.
    pub percent: Option<f32>,
    /// A short human-readable status line.
    pub message: Option<String>,
}

impl Progress {
    /// Progress with a percentage.
    #[must_use]
    pub fn percent(percent: f32) -> Progress {
        Progress {
            percent: Some(percent),
            message: None,
        }
    }

    /// Adds a status message.
    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Progress {
        self.message = Some(message.into());
        self
    }
}

impl ToJson for Progress {
    fn to_json(&self) -> Value {
        // Widening 33.3f32 to f64 gives 33.29999923706055; going through the
        // f32's shortest decimal form keeps "33.3" on the wire.
        let percent = self
            .percent
            .map(|p| p.to_string().parse::<f64>().unwrap_or(f64::from(p)));
        ObjectBuilder::new()
            .opt_field("percent", &percent)
            .opt_field("message", &self.message)
            .build()
    }
}

impl FromJson for Progress {
    fn from_json(value: &Value) -> Result<Progress, FromJsonError> {
        let r = ObjectReader::new(value, "Progress")?;
        let progress = Progress {
            percent: r.opt("percent")?,
            message: r.opt("message")?,
        };
        r.deny_unknown_fields()?;
        Ok(progress)
    }
}

impl Validate for Progress {
    fn validate_into(&self, v: &mut Validator) {
        if let Some(p) = self.percent {
            v.check((0.0..=100.0).contains(&p), "percent", || {
                format!("must be between 0 and 100, got {p}")
            });
        }
        if let Some(message) = &self.message {
            v.check_bytes("message", message, MAX_PROGRESS_MESSAGE_BYTES);
        }
    }
}

/// Error kinds follow the label rule; shared by fail requests.
pub(crate) fn validate_error_kind(v: &mut Validator, kind: &str) {
    v.check_name("kind", kind, LABEL_RULE.max(MAX_ERROR_KIND_LEN));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::UniqueScope;
    use belay_json::json;

    fn t(ms: i64) -> Timestamp {
        Timestamp::from_millis(1_700_000_000_000 + ms)
    }

    fn worker() -> WorkerId {
        WorkerId::from_static("w1")
    }

    fn running_job() -> Job {
        let mut job = Job::new(
            "01HQ3K9Z8N6V2T4R5S7W0X1Y2Z".parse().unwrap(),
            JobSpec::new("emails", "send").max_attempts(3),
            t(0),
        );
        job.transition(JobState::Running, t(10)).unwrap();
        job.attempt = 1;
        job.started_at = Some(t(10));
        job.lease = Some(Lease::new(
            worker(),
            LeaseToken::new(0xabc),
            t(10),
            Duration::from_secs(30),
        ));
        job
    }

    #[test]
    fn new_jobs_start_scheduled_or_ready() {
        let id = JobId::generate();
        let now = t(0);
        let ready = Job::new(id, JobSpec::new("q", "k"), now);
        assert_eq!((ready.state, ready.run_at), (JobState::Ready, now));
        let later = Job::new(
            id,
            JobSpec::new("q", "k").delay(Duration::from_secs(5)),
            now,
        );
        assert_eq!(later.state, JobState::Scheduled);
        assert_eq!(later.run_at, t(5_000));
        let past = Job::new(id, JobSpec::new("q", "k").run_at(t(-1)), now);
        assert_eq!(past.state, JobState::Ready);
        assert_eq!(past.run_at, t(-1));
    }

    #[test]
    fn lease_expires_at_its_instant() {
        let job = running_job();
        assert!(!job.lease_expired(t(30_009)));
        assert!(job.lease_expired(t(30_010)));
        let mut orphan = running_job();
        orphan.lease = None;
        assert!(orphan.lease_expired(t(0)));
        let ready = Job::new(JobId::generate(), JobSpec::new("q", "k"), t(0));
        assert!(!ready.lease_expired(t(1_000_000)));
    }

    #[test]
    fn heartbeats_never_shorten_a_lease() {
        let mut lease = Lease::new(worker(), LeaseToken::new(1), t(0), Duration::from_secs(60));
        lease.renew(t(1_000), Duration::from_secs(10));
        assert_eq!(lease.expires_at, t(60_000));
        assert_eq!(lease.heartbeat_at, t(1_000));
        lease.renew(t(55_000), Duration::from_secs(10));
        assert_eq!(lease.expires_at, t(65_000));
        assert_eq!(lease.remaining(t(60_000)), Duration::from_secs(5));
        assert_eq!(lease.remaining(t(70_000)), Duration::ZERO);
    }

    #[test]
    fn failure_outcome_depends_on_attempts_and_cancellation() {
        let mut job = running_job();
        assert_eq!(job.state_after_failure(true), JobState::Retrying);
        assert_eq!(job.state_after_failure(false), JobState::Dead);
        job.attempt = 3;
        assert_eq!(job.attempts_left(), 0);
        assert_eq!(job.state_after_failure(true), JobState::Dead);
        job.cancel_requested = true;
        assert_eq!(job.state_after_failure(true), JobState::Cancelled);
    }

    #[test]
    fn retry_delay_prefers_the_jobs_policy() {
        let mut job = running_job();
        let queue_policy = BackoffPolicy::constant(Duration::from_secs(7));
        let mut rng = Rng::seed_from_u64(1);
        assert_eq!(
            job.retry_delay(&queue_policy, &mut rng),
            Duration::from_secs(7)
        );
        job.spec.retry = Some("exp:1s*2".parse().unwrap());
        job.attempt = 3;
        assert_eq!(
            job.retry_delay(&queue_policy, &mut rng),
            Duration::from_secs(4)
        );
    }

    #[test]
    fn transitions_keep_bookkeeping_consistent() {
        let mut job = running_job();
        job.progress = Some(Progress::percent(50.0));
        job.cancel_requested = true;
        job.transition(JobState::Retrying, t(20)).unwrap();
        assert!(job.lease.is_none() && job.progress.is_none() && !job.cancel_requested);
        assert_eq!(job.finished_at, None);
        job.transition(JobState::Ready, t(30)).unwrap();
        job.transition(JobState::Cancelled, t(40)).unwrap();
        assert_eq!(job.finished_at, Some(t(40)));
        job.transition(JobState::Ready, t(50)).unwrap();
        assert_eq!((job.finished_at, job.updated_at), (None, t(50)));
        let err = job.transition(JobState::Succeeded, t(60)).unwrap_err();
        assert_eq!(err.to_string(), "cannot move job from ready to succeeded");
        assert_eq!(
            job.state,
            JobState::Ready,
            "a rejected transition changes nothing"
        );
        assert_eq!(job.updated_at, t(50));
    }

    #[test]
    fn error_history_is_bounded_and_keeps_the_newest() {
        let mut job = running_job();
        for attempt in 1..=15 {
            job.record_error(JobError::new(
                t(attempt.into()),
                attempt,
                format!("boom {attempt}"),
            ));
        }
        assert_eq!(job.errors.len(), MAX_ERROR_HISTORY);
        assert_eq!(job.errors[0].attempt, 6);
        assert_eq!(job.last_error().map(|e| e.attempt), Some(15));
    }

    #[test]
    fn long_error_messages_are_truncated_on_char_boundaries() {
        let long = "é".repeat(MAX_ERROR_MESSAGE_BYTES);
        let e = JobError::new(t(0), 1, long);
        assert!(e.message.len() <= MAX_ERROR_MESSAGE_BYTES);
        assert!(e.message.ends_with("é..."));
        assert_eq!(JobError::new(t(0), 1, "short").message, "short");
    }

    #[test]
    fn job_json_round_trips_with_all_fields() {
        let mut job = running_job();
        job.spec = job
            .spec
            .clone()
            .unique("k", UniqueScope::For(Duration::from_secs(60)));
        job.progress = Some(Progress::percent(33.3).with_message("halfway-ish"));
        job.cancel_requested = true;
        job.record_error(
            JobError::new(t(5), 1, "timeout")
                .kind("timeout")
                .retryable(false)
                .worker(worker()),
        );
        job.result = Some(Value::Null);
        let v = job.to_json();
        assert_eq!(
            v["progress"],
            json!({"percent": 33.3, "message": "halfway-ish"})
        );
        assert_eq!(v["lease"]["token"], "0000000000000abc");
        assert_eq!(v["result"], Value::Null);
        assert_eq!(Job::from_json(&v), Ok(job));
    }

    #[test]
    fn minimal_job_json_omits_empty_fields() {
        let job = Job::new(
            "01HQ3K9Z8N6V2T4R5S7W0X1Y2Z".parse().unwrap(),
            JobSpec::new("q", "k"),
            Timestamp::from_millis(0),
        );
        assert_eq!(
            job.to_json().to_string(),
            concat!(
                r#"{"id":"01HQ3K9Z8N6V2T4R5S7W0X1Y2Z","state":"ready","attempt":0,"#,
                r#""spec":{"queue":"q","kind":"k","priority":0},"#,
                r#""created_at":"1970-01-01T00:00:00.000Z","updated_at":"1970-01-01T00:00:00.000Z","#,
                r#""run_at":"1970-01-01T00:00:00.000Z"}"#
            )
        );
        assert_eq!(Job::from_json(&job.to_json()), Ok(job));
    }

    #[test]
    fn redacted_jobs_hide_the_token_on_the_wire() {
        let job = running_job().redacted();
        let v = job.to_json();
        assert!(v["lease"].get("token").is_none());
        let back = Job::from_json(&v).unwrap();
        assert_eq!(back.lease.map(|l| l.token), Some(LeaseToken::REDACTED));
    }

    #[test]
    fn job_records_tolerate_unknown_fields_but_not_bad_ones() {
        let mut v = running_job().to_json();
        v.as_object_mut().unwrap().insert("shard", 3);
        assert!(Job::from_json(&v).is_ok());
        v.as_object_mut().unwrap().insert("state", "sleeping");
        assert_eq!(
            Job::from_json(&v).unwrap_err().to_string(),
            r#"$.state: unknown job state "sleeping", expected one of: scheduled, ready, running, retrying, succeeded, dead, cancelled"#
        );
    }

    #[test]
    fn progress_validation_and_strictness() {
        assert!(Progress::percent(100.0).validate().is_ok());
        let err = Progress::percent(120.0).validate().unwrap_err();
        assert_eq!(
            err.to_string(),
            "$.percent: must be between 0 and 100, got 120"
        );
        assert!(Progress::percent(f32::NAN).validate().is_err());
        let err = Progress::from_json(&json!({"pct": 5})).unwrap_err();
        assert_eq!(
            err.to_string(),
            r#"$: unknown field "pct" in Progress, expected one of: percent, message"#
        );
    }

    #[test]
    fn deadline_and_runtime() {
        let mut job = running_job();
        assert_eq!(job.deadline(), None);
        job.spec.timeout = Some(Duration::from_secs(60));
        assert_eq!(job.deadline(), Some(t(60_010)));
        assert_eq!(job.runtime(), None);
        job.transition(JobState::Succeeded, t(2_010)).unwrap();
        assert_eq!(job.runtime(), Some(Duration::from_secs(2)));
        assert_eq!(job.age(t(5_000)), Duration::from_secs(5));
    }
}
