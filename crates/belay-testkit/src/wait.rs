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
