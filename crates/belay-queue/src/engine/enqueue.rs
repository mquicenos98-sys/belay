//! Enqueueing: validation, queue defaults, uniqueness and the initial
//! state.
//!
//! # Uniqueness
//!
//! A spec with `unique` claims its key within its queue when it is
//! created. The claim lasts as long as its scope says:
//!
//! - `until_started`: while the job is scheduled or ready;
//! - `until_finished`: until the job reaches a terminal state;
//! - `for:<d>`: for `d` after enqueue, whatever happens to the job, even
//!   after retention deletes it.
//!
//! Claims are taken at enqueue only. A job sent back to `ready` by
//! [`retry`](QueueEngine::retry) or a retry after failure does not claim
//! its key again: it may have been taken by a newer job in the meantime,
//! and the retried job is already known to exist.
//!
//! On a conflict with a held key, `on_conflict` decides:
//!
//! | policy | holder pending (scheduled, ready, retrying) | holder running or finished |
//! |---|---|---|
//! | `reject` | [`Error::UniqueConflict`] | [`Error::UniqueConflict`] |
//! | `return_existing` | `duplicate`, holder's id | `duplicate`, holder's id |
//! | `replace` | holder cancelled, new job `replaced` | `duplicate`, holder's id |

use belay_proto::api::{EnqueueResult, EnqueueStatus};
use belay_proto::event::EventKind;
use belay_proto::limits::MAX_ENQUEUE_BATCH;
use belay_proto::{Job, JobId, JobSpec, JobState, OnConflict, UniqueScope, Validate, Validator};

use super::QueueEngine;
use crate::error::{Error, Result};
use crate::queue::throughput::Metric;
use crate::records::Claim;
use crate::txn::Ctx;

impl QueueEngine {
    /// Validates and enqueues one job.
    ///
    /// Unset limits (`max_attempts`, `retry`, `timeout`) are filled from
    /// the queue's configuration, which is created with defaults on the
    /// queue's first use. The job starts `scheduled` if its `run_at` or
    /// `delay` puts it in the future, `ready` otherwise.
    ///
    /// ```
    /// use std::sync::Arc;
    /// use std::time::Duration;
    /// use belay_common::{ManualClock, Timestamp};
    /// use belay_proto::api::EnqueueStatus;
    /// use belay_proto::source::NoopSink;
    /// use belay_proto::{JobSpec, JobState, UniqueScope};
    /// use belay_queue::{EngineOptions, QueueEngine};
    /// use belay_store::MemKv;
    ///
    /// let clock = ManualClock::new(Timestamp::from_secs(1_700_000_000));
    /// let engine = QueueEngine::open(Arc::new(MemKv::new()), clock.shared(), NoopSink::shared(), EngineOptions::new())?;
    ///
    /// let spec = JobSpec::new("emails", "send_welcome")
    ///     .delay(Duration::from_secs(60))
    ///     .unique("user:42", UniqueScope::UntilFinished);
    /// let first = engine.enqueue(spec.clone())?;
    /// assert_eq!((first.status, first.state), (EnqueueStatus::Created, JobState::Scheduled));
    ///
    /// let again = engine.enqueue(spec)?;
    /// assert_eq!((again.status, again.id), (EnqueueStatus::Duplicate, first.id));
    /// # Ok::<(), belay_queue::Error>(())
    /// ```
    pub fn enqueue(&self, spec: JobSpec) -> Result<EnqueueResult> {
        spec.validate()?;
        self.transact(|cx| stage_enqueue(cx, spec))
    }

    /// Enqueues several jobs atomically: either every spec is accepted (as
    /// created, duplicate or replaced) and written in one batch, or none
    /// is. Later specs see earlier ones, so two specs with the same
    /// uniqueness key behave as if enqueued one after the other.
    ///
    /// Validation problems are reported for all specs at once, with paths
    /// like `$.jobs[3].priority`. An empty list enqueues nothing.
    pub fn enqueue_batch(&self, specs: Vec<JobSpec>) -> Result<Vec<EnqueueResult>> {
        if specs.is_empty() {
            return Ok(Vec::new());
        }
        let mut v = Validator::new();
        if specs.len() > MAX_ENQUEUE_BATCH {
            v.report(
                "jobs",
                format!(
                    "at most {MAX_ENQUEUE_BATCH} jobs per batch, got {}",
                    specs.len()
                ),
            );
        } else {
            v.nested("jobs", &specs);
        }
        v.finish()?;
        self.transact(|cx| {
            specs
                .into_iter()
                .map(|spec| stage_enqueue(cx, spec))
                .collect()
        })
    }
}
