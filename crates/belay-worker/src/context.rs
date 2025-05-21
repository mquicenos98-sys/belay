//! [`JobContext`]: everything a handler gets besides its own state — the
//! job, its attempt and deadline, cancellation, progress reporting and
//! the [`Extensions`] that middleware fills in.
//!
//! Cancellation is cooperative. The worker sets the flag when a heartbeat
//! answer says someone asked to cancel the job, when the attempt runs past
//! its timeout, when the lease is lost, and when a shutdown's grace period
//! runs out; the handler notices through [`JobContext::is_cancelled`] or
//! [`JobContext::sleep_or_cancel`] and returns
//! [`HandlerError::cancelled`].

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use belay_common::sync::{lock, wait_timeout, Shutdown};
use belay_json::{FromJson, Value};
use belay_proto::limits::MAX_PROGRESS_MESSAGE_BYTES;
use belay_proto::{Job, JobId, Progress, QueueName, WorkerId};

use crate::extensions::Extensions;
use crate::handler::HandlerError;

/// Why a running job was asked to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CancelReason {
    /// Someone cancelled the job; the worker learnt it from a heartbeat.
    Requested,
    /// The attempt ran past its timeout.
    TimedOut,
    /// The worker is shutting down and the grace period is over.
    ShuttingDown,
    /// The lease was lost (it expired or another worker holds it), so any
    /// result will be dropped.
    LeaseLost,
}

impl fmt::Display for CancelReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CancelReason::Requested => "cancellation requested",
            CancelReason::TimedOut => "timed out",
            CancelReason::ShuttingDown => "worker shutting down",
            CancelReason::LeaseLost => "lease lost",
        })
    }
}

/// State shared between a job's context and the worker threads watching
/// it: the cancellation flag and the progress waiting for a heartbeat.
#[derive(Debug, Default)]
pub(crate) struct JobSignals {
    /// Fast path for `is_cancelled`, set after `reason`.
    cancelled: AtomicBool,
    reason: Mutex<Option<CancelReason>>,
    wake: Condvar,
    progress: Mutex<Option<Progress>>,
}

impl JobSignals {
    /// Sets the cancellation flag. Returns `true` if this call set it; the
    /// first reason sticks.
    pub(crate) fn cancel(&self, reason: CancelReason) -> bool {
        let mut slot = lock(&self.reason);
        if slot.is_some() {
            return false;
        }
        *slot = Some(reason);
        self.cancelled.store(true, Ordering::SeqCst);
        drop(slot);
        self.wake.notify_all();
        true
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub(crate) fn reason(&self) -> Option<CancelReason> {
        *lock(&self.reason)
    }

    /// Sleeps for `d` unless cancelled first; `true` if the whole time
    /// passed.
    fn sleep(&self, d: Duration) -> bool {
        let deadline = Instant::now().checked_add(d);
        let mut slot = lock(&self.reason);
        loop {
            if slot.is_some() {
                return false;
            }
            let remaining = match deadline {
                Some(at) => match at.checked_duration_since(Instant::now()) {
                    Some(r) if !r.is_zero() => r,
                    _ => return true,
                },
                // Too far in the future to represent: wait in long steps.
                None => Duration::from_secs(3600),
            };
            slot = wait_timeout(&self.wake, slot, remaining).0;
        }
    }

    pub(crate) fn set_progress(&self, progress: Progress) {
        *lock(&self.progress) = Some(progress);
    }

    /// Takes the progress not yet sent, for a heartbeat.
    pub(crate) fn take_progress(&self) -> Option<Progress> {
        lock(&self.progress).take()
    }

    fn peek_progress(&self) -> Option<Progress> {
        lock(&self.progress).clone()
    }
}

/// Cancels a job from outside its handler; see
/// [`JobContext::cancel_handle`].
#[derive(Clone, Debug)]
pub struct CancelHandle(Arc<JobSignals>);

impl CancelHandle {
    /// Sets the job's cancellation flag with [`CancelReason::Requested`].
    /// Returns `false` if it was already cancelled.
    pub fn cancel(&self) -> bool {
        self.0.cancel(CancelReason::Requested)
    }

    /// Whether the job has been cancelled, for any reason.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
}

/// What a [`Handler`](crate::Handler) knows about the job it runs.
///
/// Log with the `job_*` macros ([`job_info!`](crate::job_info) and
/// friends) to tag records with the job id, kind and attempt.
pub struct JobContext {
    job: Arc<Job>,
    worker: WorkerId,
    signals: Arc<JobSignals>,
    deadline: Option<Instant>,
    draining: Option<Shutdown>,
    extensions: Extensions,
}

impl JobContext {
    pub(crate) fn new(
        job: Arc<Job>,
        worker: WorkerId,
        signals: Arc<JobSignals>,
        deadline: Option<Instant>,
        draining: Option<Shutdown>,
    ) -> JobContext {
        JobContext {
            job,
            worker,
            signals,
            deadline,
            draining,
            extensions: Extensions::new(),
        }
    }

    /// A context outside any worker, for calling handlers directly: in
    /// tests, or to run a job inline. Nothing watches it, so it has no
    /// deadline unless given one, and it is cancelled only through
    /// [`cancel_handle`](Self::cancel_handle).
    ///
    /// ```
    /// use belay_common::Timestamp;
    /// use belay_json::json;
    /// use belay_proto::{Job, JobId, JobSpec};
    /// use belay_worker::builtin::EchoHandler;
    /// use belay_worker::{Handler, JobContext};
    ///
    /// let spec = JobSpec::new("default", "echo").payload(json!({"n": 1}));
    /// let mut ctx = JobContext::detached(Job::new(JobId::generate(), spec, Timestamp::now()));
    /// let outcome = EchoHandler.handle(&mut ctx).unwrap();
    /// assert_eq!(outcome.result(), Some(&json!({"n": 1})));
    /// ```
    #[must_use]
    pub fn detached(job: Job) -> JobContext {
        JobContext::new(
            Arc::new(job),
            WorkerId::from_static("detached"),
            Arc::new(JobSignals::default()),
            None,
            None,
        )
    }

    /// Sets the deadline; for [`detached`](Self::detached) contexts.
    #[must_use]
    pub fn with_deadline(mut self, deadline: Instant) -> JobContext {
        self.deadline = Some(deadline);
        self
    }

    /// The job as leased: spec, attempt counter, error history.
    #[must_use]
    pub fn job(&self) -> &Job {
        &self.job
    }

    /// The job's id.
    #[must_use]
    pub fn id(&self) -> JobId {
        self.job.id
    }

    /// The handler name the job was dispatched on.
    #[must_use]
    pub fn kind(&self) -> &str {
        self.job.kind()
    }

    /// The queue the job came from.
    #[must_use]
    pub fn queue(&self) -> &QueueName {
        self.job.queue()
    }

    /// This attempt's number, starting at 1.
    #[must_use]
    pub fn attempt(&self) -> u32 {
        self.job.attempt
    }

    /// Whether a failure of this attempt would be final.
    #[must_use]
    pub fn is_last_attempt(&self) -> bool {
        self.job.attempt >= self.job.max_attempts()
    }

    /// The worker running the job.
    #[must_use]
    pub fn worker(&self) -> &WorkerId {
        &self.worker
    }

    /// The raw payload.
    #[must_use]
    pub fn payload(&self) -> &Value {
        &self.job.spec.payload
    }

    /// Decodes the payload. A mismatch is a non-retryable failure of kind
    /// [`invalid_payload`](crate::failure_kind::INVALID_PAYLOAD), so `?`
    /// does the right thing.
    pub fn payload_as<T: FromJson>(&self) -> Result<T, HandlerError> {
        T::from_json(self.payload()).map_err(HandlerError::from)
    }

    /// When this attempt's timeout runs out, on the worker's monotonic
    /// clock: the shorter of the job's `timeout` and the kind's
    /// [`KindOptions::timeout`](crate::KindOptions::timeout), counted from
    /// the moment the handler started.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Time left before the deadline; zero once it has passed.
    #[must_use]
    pub fn remaining(&self) -> Option<Duration> {
        self.deadline
            .map(|d| d.saturating_duration_since(Instant::now()))
    }

    /// Records progress; the worker sends the latest value with the next
    /// heartbeat. `percent` is clamped to `0..=100` (NaN means "unknown"),
    /// an empty `message` is omitted and a long one is cut to the
    /// protocol's limit.
    pub fn progress(&self, percent: f32, message: impl Into<String>) {
        let message = message.into();
        self.set_progress(Progress {
            percent: (!percent.is_nan()).then(|| percent.clamp(0.0, 100.0)),
            message: (!message.is_empty()).then_some(message),
        });
    }

    /// Records a full [`Progress`] value, normalised like
    /// [`progress`](Self::progress).
    pub fn set_progress(&self, mut progress: Progress) {
        progress.percent = progress
            .percent
            .filter(|p| !p.is_nan())
            .map(|p| p.clamp(0.0, 100.0));
        if let Some(message) = &mut progress.message {
            truncate_at_char(message, MAX_PROGRESS_MESSAGE_BYTES);
        }
        self.signals.set_progress(progress);
    }

    /// The latest progress not yet delivered by a heartbeat.
    #[must_use]
    pub fn pending_progress(&self) -> Option<Progress> {
        self.signals.peek_progress()
    }

    /// Whether the handler should stop. See [`cancel_reason`](Self::cancel_reason).
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.signals.is_cancelled()
    }

    /// Why the job was cancelled, if it was.
    #[must_use]
    pub fn cancel_reason(&self) -> Option<CancelReason> {
        self.signals.reason()
    }

    /// `Err(HandlerError::cancelled())` once cancelled, for `?` at
    /// convenient checkpoints.
    pub fn check_cancelled(&self) -> Result<(), HandlerError> {
        if self.is_cancelled() {
            Err(HandlerError::cancelled())
        } else {
            Ok(())
        }
    }

    /// Sleeps for `d`, waking early if the job is cancelled. Returns `true`
    /// if the full time passed and `false` if cancelled.
    ///
    /// ```
    /// use std::time::Duration;
    /// # use belay_common::Timestamp;
    /// # use belay_proto::{Job, JobId, JobSpec};
    /// use belay_worker::JobContext;
    ///
    /// # let job = Job::new(JobId::generate(), JobSpec::new("default", "poll"), Timestamp::now());
    /// let ctx = JobContext::detached(job);
    /// ctx.cancel_handle().cancel();
    /// // Returns at once instead of after an hour.
    /// assert!(!ctx.sleep_or_cancel(Duration::from_secs(3600)));
    /// ```
    #[must_use]
    pub fn sleep_or_cancel(&self, d: Duration) -> bool {
        self.signals.sleep(d)
    }

    /// Whether the worker has begun shutting down. Running jobs still get
    /// the grace period, but a long job may want to checkpoint and fail
    /// with a short `retry_after` rather than race the deadline.
    #[must_use]
    pub fn is_shutting_down(&self) -> bool {
        self.draining.as_ref().is_some_and(Shutdown::is_triggered)
    }

    /// A handle that can cancel this job from another thread.
    #[must_use]
    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle(Arc::clone(&self.signals))
    }

    /// Values attached by middleware.
    #[must_use]
    pub fn extensions(&self) -> &Extensions {
        &self.extensions
    }

    /// Values attached by middleware, mutably.
    pub fn extensions_mut(&mut self) -> &mut Extensions {
        &mut self.extensions
    }
}

impl fmt::Debug for JobContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobContext")
            .field("job", &self.job.id)
            .field("kind", &self.job.kind())
            .field("attempt", &self.job.attempt)
            .field("worker", &self.worker)
            .field("deadline", &self.deadline)
            .field("cancelled", &self.cancel_reason())
            .field("extensions", &self.extensions)
            .finish()
    }
}

/// Cuts `s` to at most `max` bytes without splitting a character.
pub(crate) fn truncate_at_char(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_common::Timestamp;
    use belay_json::json;
    use belay_proto::JobSpec;
    use std::thread;

    fn ctx_with(payload: Value) -> JobContext {
        let mut job = Job::new(
            JobId::generate(),
            JobSpec::new("default", "test")
                .payload(payload)
                .max_attempts(3),
            Timestamp::from_secs(1_700_000_000),
        );
        job.attempt = 2;
        JobContext::detached(job)
    }

    #[test]
    fn exposes_the_job_and_attempt() {
        let ctx = ctx_with(json!({"user": 42}));
        assert_eq!(ctx.kind(), "test");
        assert_eq!(ctx.queue(), "default");
        assert_eq!(ctx.attempt(), 2);
        assert!(!ctx.is_last_attempt());
        assert_eq!(ctx.payload()["user"], 42);
        assert_eq!(ctx.worker(), "detached");
        assert_eq!(ctx.deadline(), None);
        assert_eq!(ctx.remaining(), None);
        assert!(!ctx.is_shutting_down());
    }

    #[test]
    fn payload_as_reports_decoding_errors_as_fatal() {
        let ctx = ctx_with(json!({"user": "x"}));
        let err = ctx
            .payload_as::<std::collections::BTreeMap<String, u64>>()
            .unwrap_err();
        assert!(!err.retryable);
        assert_eq!(
            err.message,
            r#"invalid payload: $.user: expected integer in range 0..=18446744073709551615, found string "x""#
        );
    }

    #[test]
    fn progress_is_clamped_and_truncated() {
        let ctx = ctx_with(Value::Null);
        ctx.progress(250.0, "");
        assert_eq!(ctx.pending_progress(), Some(Progress::percent(100.0)));
        ctx.progress(f32::NAN, "é".repeat(MAX_PROGRESS_MESSAGE_BYTES));
        let p = ctx.pending_progress().expect("progress was set");
        assert_eq!(p.percent, None);
        let message = p.message.expect("message was set");
        assert!(message.len() <= MAX_PROGRESS_MESSAGE_BYTES);
        assert!(message.chars().all(|c| c == 'é'));
    }

    #[test]
    fn first_cancel_reason_sticks() {
        let ctx = ctx_with(Value::Null);
        assert!(ctx.check_cancelled().is_ok());
        let handle = ctx.cancel_handle();
        assert!(handle.cancel());
        assert!(!ctx.signals.cancel(CancelReason::TimedOut));
        assert!(handle.is_cancelled());
        assert_eq!(ctx.cancel_reason(), Some(CancelReason::Requested));
        assert!(ctx.check_cancelled().unwrap_err().is_cancellation());
    }

    #[test]
    fn sleep_or_cancel_wakes_when_cancelled_from_another_thread() {
        let ctx = ctx_with(Value::Null);
        let handle = ctx.cancel_handle();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            handle.cancel();
        });
        let started = Instant::now();
        assert!(!ctx.sleep_or_cancel(Duration::from_secs(30)));
        assert!(started.elapsed() < Duration::from_secs(10));
        canceller.join().expect("canceller thread");
    }

    #[test]
    fn sleep_or_cancel_completes_short_sleeps() {
        let ctx = ctx_with(Value::Null);
        assert!(ctx.sleep_or_cancel(Duration::from_millis(2)));
        assert!(ctx.sleep_or_cancel(Duration::ZERO));
    }

    #[test]
    fn deadlines_count_down_to_zero() {
        let ctx = ctx_with(Value::Null).with_deadline(Instant::now());
        assert_eq!(ctx.remaining(), Some(Duration::ZERO));
        let ctx = ctx_with(Value::Null).with_deadline(Instant::now() + Duration::from_secs(60));
        assert!(ctx.remaining().expect("deadline set") > Duration::from_secs(50));
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        let mut s = String::from("aé");
        truncate_at_char(&mut s, 2);
        assert_eq!(s, "a");
        let mut s = String::from("abc");
        truncate_at_char(&mut s, 10);
        assert_eq!(s, "abc");
    }
}
