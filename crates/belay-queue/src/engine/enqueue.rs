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

/// Stages one enqueue.
fn stage_enqueue(cx: &mut Ctx<'_>, mut spec: JobSpec) -> Result<EnqueueResult> {
    let now = cx.now;
    let record = cx.ensure_queue(&spec.queue);
    spec.apply_queue_defaults(&record.config);

    let mut replaced = false;
    if let Some(unique) = spec.unique.clone() {
        if let Some((claim, holder_state)) = live_claim(cx, &spec, &unique.key) {
            let pending = !holder_state.is_terminal();
            match unique.on_conflict {
                OnConflict::Reject => {
                    return Err(Error::UniqueConflict {
                        queue: spec.queue,
                        key: unique.key,
                        holder: claim.job,
                    });
                }
                OnConflict::Replace if pending => {
                    // A pending holder that was purged since has nothing
                    // left to cancel; the new job simply takes its key.
                    if cx.job_state(claim.job).is_some() {
                        cancel_holder(cx, claim.job)?;
                    }
                    replaced = true;
                }
                OnConflict::ReturnExisting | OnConflict::Replace => {
                    cx.counters().duplicates += 1;
                    return Ok(EnqueueResult {
                        id: claim.job,
                        status: EnqueueStatus::Duplicate,
                        state: holder_state,
                    });
                }
            }
        }
    }

    let id = cx.job_id()?;
    let job = Job::new(id, spec, now);
    if let Some(unique) = &job.spec.unique {
        let until = match unique.scope {
            UniqueScope::For(period) => Some(now + period),
            UniqueScope::UntilStarted | UniqueScope::UntilFinished => None,
        };
        cx.set_claim(&job.spec.queue, &unique.key, Some(Claim::new(id, until)));
    }
    cx.emit(EventKind::JobEnqueued {
        job: id,
        queue: job.spec.queue.clone(),
        kind: job.spec.kind.clone(),
        run_at: job.run_at,
    });
    cx.metric(&job.spec.queue, Metric::Enqueued);
    let counters = cx.counters();
    counters.enqueued += 1;
    if replaced {
        counters.replaced += 1;
    }
    if job.state == JobState::Ready {
        cx.notify();
    }
    let result = EnqueueResult {
        id,
        status: if replaced {
            EnqueueStatus::Replaced
        } else {
            EnqueueStatus::Created
        },
        state: job.state,
    };
    cx.put(job);
    Ok(result)
}

/// The claim on `key` in the spec's queue if it is still held, with the
/// holder's current (or, once deleted, last) state.
fn live_claim(cx: &Ctx<'_>, spec: &JobSpec, key: &str) -> Option<(Claim, JobState)> {
    let claim = cx.claim(&spec.queue, key)?;
    if claim.expired(cx.now) {
        // Timed out but not yet swept by tick: free to take.
        return None;
    }
    let state = cx.job_state(claim.job).or(claim.holder_state)?;
    Some((claim, state))
}
