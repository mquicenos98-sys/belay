//! Waiting for the system under test: [`eventually`] polls a condition,
//! [`wait_for_state`] and friends poll the HTTP API for a job or a
//! workflow run.
//!
//! Polling starts every millisecond and backs off to every 25 ms, so a
//! condition that holds quickly is noticed quickly while a slow one does
//! not burn a core. Timeouts are generous by design: they bound how long a
//! broken test hangs, they are not a statement about performance. A
//! timeout error says what was awaited and, for jobs, the state last seen.

use std::thread;
use std::time::{Duration, Instant};

use belay_client::Client;
use belay_proto::workflow::WorkflowState;
use belay_proto::{Job, JobId, JobState, Workflow, WorkflowId};

use crate::error::{Error, Result};

/// A timeout for waits in tests: long enough to ride out a loaded CI
/// machine, short enough that a broken test fails in reasonable time.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

const FIRST_NAP: Duration = Duration::from_millis(1);

const LONGEST_NAP: Duration = Duration::from_millis(25);

/// Polls `condition` until it returns `true` or `timeout` passes.
///
/// ```
/// use std::sync::atomic::{AtomicU32, Ordering};
/// use std::time::Duration;
/// use belay_testkit::eventually;
///
/// let calls = AtomicU32::new(0);
/// eventually(Duration::from_secs(5), || calls.fetch_add(1, Ordering::SeqCst) >= 3)?;
/// assert!(eventually(Duration::from_millis(10), || false).unwrap_err().is_timeout());
/// # Ok::<(), belay_testkit::Error>(())
/// ```
pub fn eventually(timeout: Duration, mut condition: impl FnMut() -> bool) -> Result<()> {
    eventually_some(timeout, "the condition to hold", || {
        condition().then_some(())
    })
}

/// Polls `probe` until it returns `Some`, and returns that value; `what`
/// describes the wait in the timeout error.
pub fn eventually_some<T>(
    timeout: Duration,
    what: &str,
    mut probe: impl FnMut() -> Option<T>,
) -> Result<T> {
    poll(timeout, || match probe() {
        Some(value) => Probe::Ready(value),
        None => Probe::Pending,
    })
    .map_err(|stop| stop.into_error(|| what.to_owned()))
}

/// Polls `GET /v1/jobs/:id` until the job is in `state`, and returns it.
///
/// Transport errors while polling count as "not yet" (a server being
/// restarted is briefly unreachable); an API error such as `not_found`
/// ends the wait at once.
pub fn wait_for_state(
    client: &Client,
    id: JobId,
    state: JobState,
    timeout: Duration,
) -> Result<Job> {
    wait_for_job(client, id, &state.to_string(), timeout, |job| {
        job.state == state
    })
}

/// Polls until the job reaches any terminal state (`succeeded`, `dead` or
/// `cancelled`), and returns it.
pub fn wait_for_terminal(client: &Client, id: JobId, timeout: Duration) -> Result<Job> {
    wait_for_job(client, id, "finished", timeout, Job::is_terminal)
}

/// Polls until `accept` holds for the job, and returns it; `what` completes
/// the sentence "waiting for job `<id>` to be ...".
pub fn wait_for_job(
    client: &Client,
    id: JobId,
    what: &str,
    timeout: Duration,
    mut accept: impl FnMut(&Job) -> bool,
) -> Result<Job> {
    let mut last: Option<Job> = None;
    let outcome = poll(timeout, || match client.jobs().get(id) {
        Ok(job) if accept(&job) => Probe::Ready(job),
        Ok(job) => {
            last = Some(job);
            Probe::Pending
        }
        Err(e) if e.api_error().is_some() => Probe::Failed(e.into()),
        Err(_) => Probe::Pending,
    });
    outcome.map_err(|stop| {
        stop.into_error(|| format!("job {id} to be {what} ({})", describe(last.as_ref())))
    })
}

/// Polls `GET /v1/workflows/:id` until the run is over, and returns it.
pub fn wait_for_workflow(client: &Client, id: WorkflowId, timeout: Duration) -> Result<Workflow> {
    let mut last: Option<WorkflowState> = None;
    let outcome = poll(timeout, || match client.workflows().get(id) {
        Ok(run) if run.state.is_terminal() => Probe::Ready(run),
        Ok(run) => {
            last = Some(run.state);
            Probe::Pending
        }
        Err(e) if e.api_error().is_some() => Probe::Failed(e.into()),
        Err(_) => Probe::Pending,
    });
    outcome.map_err(|stop| {
        stop.into_error(|| {
            let seen = last.map_or_else(|| "never read".to_owned(), |s| format!("last seen {s}"));
            format!("workflow {id} to finish ({seen})")
        })
    })
}

/// One look at the condition being waited for.
enum Probe<T> {
    Ready(T),
    Pending,
    Failed(Error),
}

/// Why [`poll`] gave up.
enum Stop {
    TimedOut(Duration),
    Failed(Error),
}
