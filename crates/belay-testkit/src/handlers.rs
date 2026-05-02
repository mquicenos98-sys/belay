//! Recording handlers: [`Recorder`], [`Execution`], [`Behavior`], [`Gate`]
//! and [`RecordingHandler`], which ties them together.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use belay_common::hash::hash64;
use belay_common::sync::{lock, wait_timeout};
use belay_json::Value;
use belay_proto::{JobId, QueueName, WorkerId};
use belay_worker::{Handler, HandlerError, JobContext, Outcome};

use crate::error::{Error, Result};

/// How often waiting handlers look at cancellation and shutdown.
const CHECK_EVERY: Duration = Duration::from_millis(5);

/// How an attempt ended, as far as the handler knows.
#[derive(Clone, Debug, PartialEq)]
pub enum Recorded {
    /// Still running.
    Running,
    /// Returned success with this result.
    Succeeded(Option<Value>),
    /// Returned an error.
    Failed {
        /// The error message.
        message: String,
        /// Whether it asked for a retry.
        retryable: bool,
    },
    /// Stopped because the job was cancelled (on request or by timeout).
    Cancelled,
}

/// One attempt of one job, as seen by a recording handler.
#[derive(Clone, Debug, PartialEq)]
pub struct Execution {
    /// Order of the start among everything this recorder saw, from 0.
    pub seq: u64,
    /// The job.
    pub job: JobId,
    /// Its kind.
    pub kind: String,
    /// Its queue.
    pub queue: QueueName,
    /// The server's attempt number (restarts at 1 after an operator retry).
    pub attempt: u32,
    /// The worker that ran it.
    pub worker: WorkerId,
    /// The payload, as resolved by the server (workflow placeholders
    /// already replaced).
    pub payload: Value,
    /// When the handler started.
    pub started: Instant,
    /// How long it ran; `None` while running.
    pub took: Option<Duration>,
    /// What it returned.
    pub outcome: Recorded,
}

impl Execution {
    /// Whether the handler returned success.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        matches!(self.outcome, Recorded::Succeeded(_))
    }
}

#[derive(Default)]
struct State {
    executions: Vec<Execution>,
    /// Index into `executions` of every attempt still running, by job.
    running: HashMap<JobId, Vec<usize>>,
    running_by_queue: BTreeMap<QueueName, usize>,
    peak_by_queue: BTreeMap<QueueName, usize>,
    /// Jobs that started while another attempt of theirs was running.
    overlaps: Vec<JobId>,
    /// Bumped on every start and finish, so waiters miss no change.
    version: u64,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

/// Collects one [`Execution`] per attempt run by a [`RecordingHandler`].
/// Clones share the same record.
///
/// A recorder can be shared by several workers, which is how tests check
/// properties across a fleet: which worker ran which queue, how many jobs
/// of a queue ran at once ([`peak_running`](Recorder::peak_running)), and
/// whether any job ever ran twice at the same time
/// ([`overlaps`](Recorder::overlaps)). It sees a handler return before the
/// worker has reported the outcome, so assertions about server state
/// should wait on the server.
#[derive(Clone, Default)]
pub struct Recorder {
    shared: Arc<Shared>,
}

impl Recorder {
    /// An empty recorder.
    #[must_use]
    pub fn new() -> Recorder {
        Recorder::default()
    }

    /// Every attempt so far, in start order.
    #[must_use]
    pub fn executions(&self) -> Vec<Execution> {
        lock(&self.shared.state).executions.clone()
    }

    /// Number of attempts started.
    #[must_use]
    pub fn len(&self) -> usize {
        lock(&self.shared.state).executions.len()
    }

    /// Whether nothing has run.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Attempts that have returned, whatever the outcome.
    #[must_use]
    pub fn finished(&self) -> usize {
        let state = lock(&self.shared.state);
        state
            .executions
            .iter()
            .filter(|e| e.outcome != Recorded::Running)
            .count()
    }

    /// Attempts that returned success.
    #[must_use]
    pub fn succeeded(&self) -> usize {
        let state = lock(&self.shared.state);
        state.executions.iter().filter(|e| e.succeeded()).count()
    }

    /// The attempts of job `id`, in start order.
    #[must_use]
    pub fn of_job(&self, id: JobId) -> Vec<Execution> {
        self.filtered(|e| e.job == id)
    }

    /// The server's attempt numbers of job `id`'s executions, in order.
    #[must_use]
    pub fn attempts_of(&self, id: JobId) -> Vec<u32> {
        self.of_job(id).iter().map(|e| e.attempt).collect()
    }

    /// The attempts of jobs of `kind`, in start order.
    #[must_use]
    pub fn of_kind(&self, kind: &str) -> Vec<Execution> {
        self.filtered(|e| e.kind == kind)
    }

    /// The payloads of `kind`'s attempts, in start order.
    #[must_use]
    pub fn payloads(&self, kind: &str) -> Vec<Value> {
        self.of_kind(kind).into_iter().map(|e| e.payload).collect()
    }

    /// Jobs with an attempt running right now.
    #[must_use]
    pub fn running(&self) -> Vec<JobId> {
        let state = lock(&self.shared.state);
        let mut ids: Vec<JobId> = state.running.keys().copied().collect();
        ids.sort();
        ids
    }

    /// Attempts of `queue`'s jobs running right now.
    #[must_use]
    pub fn running_in(&self, queue: &str) -> usize {
        let state = lock(&self.shared.state);
        count_for(&state.running_by_queue, queue)
    }

    /// The most attempts of `queue`'s jobs that ever ran at once.
    #[must_use]
    pub fn peak_running(&self, queue: &str) -> usize {
        let state = lock(&self.shared.state);
        count_for(&state.peak_by_queue, queue)
    }

    /// Jobs that started an attempt while another attempt of theirs was
    /// still running: a job leased to two workers at once. Empty in a
    /// correct system.
    #[must_use]
    pub fn overlaps(&self) -> Vec<JobId> {
        lock(&self.shared.state).overlaps.clone()
    }

    /// Blocks until `condition` holds for this recorder (it is checked
    /// whenever an attempt starts or ends) or `timeout` passes.
    ///
    /// ```
    /// use std::time::Duration;
    /// use belay_testkit::Recorder;
    ///
    /// let recorder = Recorder::new();
    /// recorder.wait_until(Duration::from_secs(1), |r| r.is_empty())?;
    /// assert!(recorder.wait_until(Duration::from_millis(10), |r| r.len() == 1).is_err());
    /// # Ok::<(), belay_testkit::Error>(())
    /// ```
    pub fn wait_until(
        &self,
        timeout: Duration,
        condition: impl Fn(&Recorder) -> bool,
    ) -> Result<()> {
        let started = Instant::now();
        loop {
            let seen = lock(&self.shared.state).version;
            if condition(self) {
                return Ok(());
            }
            let state = lock(&self.shared.state);
            let waited = started.elapsed();
            if waited >= timeout {
                return Err(Error::timeout(
                    format!(
                        "the recorder condition ({} attempts started, {} running)",
                        state.executions.len(),
                        state.running.len()
                    ),
                    waited,
                ));
            }
            if state.version == seen {
                // Bounded, so a condition that also looks at something
                // other than this recorder is re-read now and then.
                let nap = (timeout - waited).min(Duration::from_millis(50));
                drop(wait_timeout(&self.shared.changed, state, nap));
            }
        }
    }

    /// Blocks until job `id` has an attempt running.
    pub fn wait_for_start(&self, id: JobId, timeout: Duration) -> Result<Execution> {
        self.wait_until(timeout, |r| {
            r.of_job(id).iter().any(|e| e.outcome == Recorded::Running)
        })
        .map_err(|_| Error::timeout(format!("job {id} to start on a worker"), timeout))?;
        self.of_job(id)
            .into_iter()
            .rev()
            .find(|e| e.outcome == Recorded::Running)
            .ok_or_else(|| Error::timeout(format!("job {id} to keep running"), timeout))
    }

    fn filtered(&self, keep: impl Fn(&Execution) -> bool) -> Vec<Execution> {
        let state = lock(&self.shared.state);
        state
            .executions
            .iter()
            .filter(|e| keep(e))
            .cloned()
            .collect()
    }

    /// Records the start of an attempt; returns its index.
    fn start(&self, ctx: &JobContext) -> usize {
        let mut state = lock(&self.shared.state);
        let index = state.executions.len();
        let queue = ctx.queue().clone();
        state.executions.push(Execution {
            seq: index as u64,
            job: ctx.id(),
            kind: ctx.kind().to_owned(),
            queue: queue.clone(),
            attempt: ctx.attempt(),
            worker: ctx.worker().clone(),
            payload: ctx.payload().clone(),
            started: Instant::now(),
            took: None,
            outcome: Recorded::Running,
        });
        let attempts = state.running.entry(ctx.id()).or_default();
        attempts.push(index);
        if attempts.len() > 1 {
            state.overlaps.push(ctx.id());
        }
        let running = state.running_by_queue.entry(queue.clone()).or_insert(0);
        *running += 1;
        let now = *running;
        let peak = state.peak_by_queue.entry(queue).or_insert(0);
        *peak = (*peak).max(now);
        state.version += 1;
        drop(state);
        self.shared.changed.notify_all();
        index
    }

    fn finish(&self, index: usize, outcome: Recorded) {
        let mut state = lock(&self.shared.state);
        let execution = &mut state.executions[index];
        execution.took = Some(execution.started.elapsed());
        execution.outcome = outcome;
        let (job, queue) = (execution.job, execution.queue.clone());
        if let Some(attempts) = state.running.get_mut(&job) {
            attempts.retain(|i| *i != index);
        }
        if let Some(running) = state.running_by_queue.get_mut(&queue) {
            *running = running.saturating_sub(1);
        }
        state.version += 1;
        drop(state);
        self.shared.changed.notify_all();
    }

    /// Attempts of job `id` this recorder saw before the current one.
    fn previous_attempts(&self, id: JobId) -> usize {
        let state = lock(&self.shared.state);
        state
            .executions
            .iter()
            .filter(|e| e.job == id && e.outcome != Recorded::Running)
            .count()
    }
}

fn count_for(map: &BTreeMap<QueueName, usize>, queue: &str) -> usize {
    map.iter()
        .find(|(name, _)| name.as_str() == queue)
        .map_or(0, |(_, n)| *n)
}

impl fmt::Debug for Recorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = lock(&self.shared.state);
        f.debug_struct("Recorder")
            .field("executions", &state.executions.len())
            .field("running", &state.running.len())
            .field("overlaps", &state.overlaps.len())
            .finish()
    }
}

/// A barrier that holds handlers until the test opens it. Clones share
/// the gate.
///
/// Waiting handlers still notice cancellation and worker shutdown, so a
/// gate that is never opened does not hang a test.
#[derive(Clone, Default)]
pub struct Gate {
    shared: Arc<(Mutex<bool>, Condvar)>,
}

impl Gate {
    /// A closed gate.
    #[must_use]
    pub fn new() -> Gate {
        Gate::default()
    }

    /// Lets every waiting and future handler through.
    pub fn open(&self) {
        *lock(&self.shared.0) = true;
        self.shared.1.notify_all();
    }

    /// Holds future handlers again.
    pub fn close(&self) {
        *lock(&self.shared.0) = false;
    }

    /// Whether the gate is open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        *lock(&self.shared.0)
    }

    /// Waits until the gate opens (`true`), or until `stop` says to give up
    /// (`false`), looking at `stop` every few milliseconds.
    fn wait_unless(&self, stop: impl Fn() -> bool) -> bool {
        let mut open = lock(&self.shared.0);
        loop {
            if *open {
                return true;
            }
            if stop() {
                return false;
            }
            open = wait_timeout(&self.shared.1, open, CHECK_EVERY).0;
        }
    }
}

impl fmt::Debug for Gate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Gate")
            .field("open", &self.is_open())
            .finish()
    }
}

/// What a recording handler does with each attempt. Every behavior that
/// succeeds returns the payload as the job's result, so tests can check
/// results (and workflow steps can template on them).
#[derive(Clone, Debug)]
pub enum Behavior {
    /// Succeed at once.
    Succeed,
    /// Fail (retryably) the first `n` times this recorder sees a job, then
    /// succeed. Counting per recorder rather than by the server's attempt
    /// number keeps working across an operator retry, which restarts the
    /// attempt count.
    FailTimes(u32),
    /// Fail every attempt, non-retryably: the job goes straight to `dead`.
    FailAlways,
    /// Sleep this long (waking early if cancelled), then succeed.
    Sleep(Duration),
    /// Wait at the gate, then succeed.
    Gated(Gate),
    /// Fail retryably with probability `probability`, decided by
    /// [`Behavior::flaky_fails`] so the same job and attempt always get the
    /// same answer.
    Flaky {
        /// Chance that an attempt fails, 0 to 1.
        probability: f64,
        /// Seed of the decision.
        seed: u64,
    },
}

impl Behavior {
    /// Whether a [`Flaky`](Behavior::Flaky) handler fails attempt `attempt`
    /// of job `job`. Deterministic, so a test can predict each job's fate.
    #[must_use]
    pub fn flaky_fails(job: JobId, attempt: u32, probability: f64, seed: u64) -> bool {
        let mut key = job.to_string().into_bytes();
        key.extend_from_slice(&attempt.to_le_bytes());
        // The top 53 bits make an exactly representable fraction in [0, 1).
        let fraction = (hash64(&key, seed) >> 11) as f64 / (1u64 << 53) as f64;
        fraction < probability
    }
}

/// A [`Handler`] that records every attempt in a [`Recorder`] and then
/// acts out its [`Behavior`].
#[derive(Clone, Debug)]
pub struct RecordingHandler {
    recorder: Recorder,
    behavior: Behavior,
}
