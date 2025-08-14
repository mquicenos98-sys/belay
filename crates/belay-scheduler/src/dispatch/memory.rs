//! An in-process [`Dispatcher`] that keeps jobs in a map.
//!
//! It models just enough of the queue for the engines to be exercised
//! without one: validation of submitted specs, unique keys with their
//! scopes and conflict policies, the start and end of a job's life, and
//! cancellation (immediate for waiting jobs, a request for running ones).
//! Tests drive jobs to completion by hand and can inject failures.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::sync::Mutex;

use belay_common::id::SyncUlidGenerator;
use belay_common::sync::lock;
use belay_common::{SharedClock, Timestamp};
use belay_json::Value;
use belay_proto::{
    Job, JobError, JobId, JobSpec, JobState, OnConflict, QueueName, Validate, WorkflowId,
};

use super::{DispatchError, DispatchOutcome, Dispatcher};

/// A dispatcher that holds jobs in memory, for tests and examples.
///
/// ```
/// use belay_common::ManualClock;
/// use belay_proto::{JobSpec, JobState, UniqueScope};
/// use belay_scheduler::{Dispatcher, MemoryDispatcher};
///
/// let clock = ManualClock::new("2025-06-01T00:00:00Z".parse().unwrap());
/// let queue = MemoryDispatcher::new(clock.shared());
/// let spec = JobSpec::new("reports", "build").unique("daily", UniqueScope::UntilFinished);
/// let first = queue.dispatch(&spec).unwrap();
/// let again = queue.dispatch(&spec).unwrap();
/// assert_eq!((again.id, again.duplicate), (first.id, true));
///
/// queue.succeed(&first.id, belay_json::json!({"rows": 12}));
/// assert_eq!(queue.job_state(&first.id), Some(JobState::Succeeded));
/// // The key was held until the job finished; now it is free again.
/// assert!(!queue.dispatch(&spec).unwrap().duplicate);
/// ```
pub struct MemoryDispatcher {
    clock: SharedClock,
    ids: SyncUlidGenerator,
    inner: Mutex<Inner>,
}
