//! The request-facing methods of [`Engine`], one per endpoint of the HTTP
//! API (the crate docs map endpoints to methods). Every failure is an
//! [`ApiError`] the server can send as is; list endpoints take a
//! [`Paging`] and return a [`Page`].

use std::path::PathBuf;
use std::time::Instant;

use belay_json::{json, ObjectBuilder, ToJson, Value};
use belay_proto::api::{
    ApiError, CheckpointResponse, Cursor, EnqueueRequest, EnqueueResponse, EnqueueResult,
    JobFilter, MemberInfo, MemberState, Page, PreviewResponse, PurgeResponse, TriggerResponse,
    WorkerInfo,
};
use belay_proto::limits::{DEFAULT_PAGE_LIMIT, MAX_PAGE_LIMIT};
use belay_proto::{
    CalendarSpec, Job, JobId, JobSpec, JobState, QueueConfig, QueueInfo, QueueName, Schedule,
    ScheduleId, ScheduleSpec, Workflow, WorkflowId, WorkflowSpec,
};
use belay_queue::VerifyReport;
use belay_scheduler::{Upserted, WorkflowDetails, WorkflowFilter};
use belay_store::KvStats;

use super::Engine;
use crate::cluster::{queue_key, schedule_key, Ownership};

/// Where a listing starts and how much it returns: the `cursor` and
/// `limit` query parameters of the list endpoints.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Paging {
    /// Continue after this position, from a previous page's
    /// `next_cursor`.
    pub cursor: Option<Cursor>,
    /// Page size, 1 to 1000; default 100.
    pub limit: Option<u32>,
}

impl Paging {
    /// The first page with the default size.
    #[must_use]
    pub fn new() -> Paging {
        Paging::default()
    }

    /// Continues after `cursor`.
    #[must_use]
    pub fn after(mut self, cursor: Cursor) -> Paging {
        self.cursor = Some(cursor);
        self
    }

    /// Sets the page size.
    #[must_use]
    pub fn limit(mut self, limit: u32) -> Paging {
        self.limit = Some(limit);
        self
    }

    /// Reads `cursor` and `limit` from decoded query pairs; other keys are
    /// left to the caller.
    ///
    /// ```
    /// use belay_core::Paging;
    ///
    /// let paging = Paging::from_pairs([("limit", "20"), ("state", "ready")]).unwrap();
    /// assert_eq!(paging.limit, Some(20));
    /// assert!(Paging::from_pairs([("limit", "0")]).is_err());
    /// assert!(Paging::from_pairs([("cursor", "garbage")]).is_err());
    /// ```
    pub fn from_pairs<'a>(
        pairs: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Paging, ApiError> {
        let mut paging = Paging::new();
        for (key, value) in pairs {
            match key {
                "cursor" if !value.is_empty() => {
                    let cursor = value
                        .parse::<Cursor>()
                        .map_err(|e| ApiError::invalid_argument(format!("$.cursor: {e}")))?;
                    paging.cursor = Some(cursor);
                }
                "limit" => {
                    let limit = value.parse::<u32>().map_err(|_| {
                        ApiError::invalid_argument(format!(
                            "$.limit: expected a number, got {value:?}"
                        ))
                    })?;
                    paging.limit = Some(limit);
                }
                _ => {}
            }
        }
        paging.effective_limit()?;
        Ok(paging)
    }

    fn effective_limit(&self) -> Result<u32, ApiError> {
        match self.limit {
            None => Ok(DEFAULT_PAGE_LIMIT),
            Some(n) if (1..=MAX_PAGE_LIMIT).contains(&n) => Ok(n),
            Some(n) => Err(ApiError::invalid_argument(format!(
                "$.limit: must be between 1 and {MAX_PAGE_LIMIT}, got {n}"
            ))),
        }
    }

    /// The name a name-ordered listing continues after.
    fn after_name(&self) -> Result<Option<&str>, ApiError> {
        match &self.cursor {
            None => Ok(None),
            Some(cursor) => cursor.as_name().map(Some).ok_or_else(|| {
                ApiError::invalid_argument("$.cursor: was not issued by this listing")
            }),
        }
    }
}

/// One page of `items` ordered by `name`, after `paging`'s cursor.
fn page_by_name<T>(
    mut items: Vec<T>,
    name: impl Fn(&T) -> &str,
    paging: &Paging,
) -> Result<Page<T>, ApiError> {
    let limit = paging.effective_limit()?;
    let after = paging.after_name()?;
    items.retain(|item| after.map_or(true, |a| name(item) > a));
    items.sort_by(|a, b| name(a).cmp(name(b)));
    items.truncate(limit as usize + 1);
    Ok(Page::paginate(items, limit, |item| {
        Cursor::from_name(name(item))
    }))
}

/// `GET /v1/admin/store`: what the store is and its figures.
#[derive(Clone, Debug, PartialEq)]
pub struct StoreReport {
    /// `memory`, `disk` or `custom`.
    pub storage: String,
    /// The data directory, for `disk`.
    pub dir: Option<PathBuf>,
    /// The store's own statistics.
    pub stats: KvStats,
}

impl ToJson for StoreReport {
    fn to_json(&self) -> Value {
        let s = &self.stats;
        let r = &s.recovery;
        let recovery = json!({
            "snapshot_seq": r.snapshot_seq,
            "snapshot_keys": r.snapshot_keys,
            "bad_snapshots": r.bad_snapshots,
            "segments_replayed": r.segments_replayed,
            "batches_replayed": r.batches_replayed,
            "torn_tails": r.torn_tails,
            "torn_bytes": r.torn_bytes,
            "skipped_regions": r.skipped_regions,
            "skipped_bytes": r.skipped_bytes,
            "removed_files": r.removed_files,
            "duration": belay_common::format_duration(r.duration),
            "lost_data": r.lost_data(),
        });
        ObjectBuilder::new()
            .field("storage", &self.storage)
            .opt_field_with("dir", &self.dir, |d| {
                Value::String(d.to_string_lossy().into_owned())
            })
            .field("keys", s.keys)
            .field("live_bytes", s.live_bytes)
            .field("last_seq", s.last_seq)
            .field("durable_seq", s.durable_seq)
            .field("batches", s.batches)
            .field("ops", s.ops)
            .field("commit_groups", s.commit_groups)
            .field("wal_syncs", s.wal_syncs)
            .field("wal_bytes_written", s.wal_bytes_written)
            .field("wal_segments", s.wal_segments)
            .field("wal_bytes", s.wal_bytes)
            .field("checkpoints", s.checkpoints)
            .field("checkpoint_seq", s.checkpoint_seq)
            .field("snapshot_bytes", s.snapshot_bytes)
            .raw("recovery", recovery)
            .build()
    }
}

/// The result of [`Engine::verify`]: whether the queue's in-memory indexes
/// agree with the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyOutcome {
    /// Jobs found in the store.
    pub jobs: u64,
    /// Queues found.
    pub queues: u64,
    /// Uniqueness claims found.
    pub claims: u64,
    /// One line per inconsistency (at most 100).
    pub problems: Vec<String>,
    /// Inconsistencies beyond those listed.
    pub more_problems: u64,
}

impl VerifyOutcome {
    /// Whether everything agrees.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        self.problems.is_empty()
    }
}

impl From<VerifyReport> for VerifyOutcome {
    fn from(r: VerifyReport) -> VerifyOutcome {
        VerifyOutcome {
            jobs: r.jobs,
            queues: r.queues,
            claims: r.claims,
            problems: r.problems,
            more_problems: r.more_problems,
        }
    }
}

impl ToJson for VerifyOutcome {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("consistent", self.is_consistent())
            .field("jobs", self.jobs)
            .field("queues", self.queues)
            .field("claims", self.claims)
            .field("problems", &self.problems)
            .field("more_problems", self.more_problems)
            .build()
    }
}

/// Jobs.
impl Engine {
    /// Enqueues one job.
    pub fn enqueue(&self, spec: JobSpec) -> Result<EnqueueResult, ApiError> {
        Ok(self.shared.queue.enqueue(spec)?)
    }

    /// Enqueues several jobs atomically: all are accepted or none.
    pub fn enqueue_batch(&self, specs: Vec<JobSpec>) -> Result<EnqueueResponse, ApiError> {
        let results = self.shared.queue.enqueue_batch(specs)?;
        Ok(EnqueueResponse { results })
    }

    /// `POST /v1/jobs` with either body form.
    pub fn enqueue_request(&self, request: EnqueueRequest) -> Result<EnqueueResponse, ApiError> {
        match request {
            EnqueueRequest::One(spec) => Ok(EnqueueResponse {
                results: vec![self.enqueue(spec)?],
            }),
            EnqueueRequest::Batch(specs) => self.enqueue_batch(specs),
        }
    }

    /// The job `id`.
    pub fn get_job(&self, id: &JobId) -> Result<Job, ApiError> {
        Ok(self.shared.queue.get(id)?)
    }

    /// One page of jobs matching `filter`.
    pub fn list_jobs(&self, filter: &JobFilter) -> Result<Page<Job>, ApiError> {
        Ok(self.shared.queue.list(filter)?)
    }

    /// Cancels a job: waiting and ready jobs at once, running ones through
    /// their worker's next heartbeat.
    pub fn cancel_job(&self, id: &JobId) -> Result<Job, ApiError> {
        Ok(self.shared.queue.cancel(id)?)
    }

    /// Sends a dead or cancelled job back to `ready`.
    pub fn retry_job(&self, id: &JobId) -> Result<Job, ApiError> {
        Ok(self.shared.queue.retry(id)?)
    }

    /// Makes a scheduled or retrying job ready now.
    pub fn promote_job(&self, id: &JobId) -> Result<Job, ApiError> {
        Ok(self.shared.queue.promote(id)?)
    }

    /// Deletes a finished job.
    pub fn delete_job(&self, id: &JobId) -> Result<(), ApiError> {
        Ok(self.shared.queue.delete(id)?)
    }
}

/// Queues.
impl Engine {
    /// One page of queues, by name.
    pub fn list_queues(&self, paging: &Paging) -> Result<Page<QueueInfo>, ApiError> {
        let queues = self.shared.queue.list_queues();
        page_by_name(queues, |q| q.name.as_str(), paging)
    }

    /// Status of queue `name`.
    pub fn queue_info(&self, name: &QueueName) -> Result<QueueInfo, ApiError> {
        Ok(self.shared.queue.queue_info(name)?)
    }

    /// Replaces a queue's configuration, creating the queue if needed.
    pub fn set_queue_config(
        &self,
        name: &QueueName,
        config: QueueConfig,
    ) -> Result<QueueInfo, ApiError> {
        Ok(self.shared.queue.set_config(name, config)?)
    }

    /// Stops leasing from a queue (creating it if needed).
    pub fn pause_queue(&self, name: &QueueName) -> Result<QueueInfo, ApiError> {
        self.shared.queue.pause(name)?;
        self.queue_info(name)
    }

    /// Resumes leasing from a queue.
    pub fn resume_queue(&self, name: &QueueName) -> Result<QueueInfo, ApiError> {
        self.shared.queue.resume(name)?;
        self.queue_info(name)
    }

    /// Deletes a queue's jobs, only those in `state` if given (never
    /// running ones).
    pub fn purge_queue(
        &self,
        name: &QueueName,
        state: Option<JobState>,
    ) -> Result<PurgeResponse, ApiError> {
        let purged = self.shared.queue.purge(name, state)?;
        Ok(PurgeResponse { purged })
    }

    /// Deletes a queue with its jobs. Without `force`, a queue with
    /// unfinished jobs is refused.
    pub fn delete_queue(&self, name: &QueueName, force: bool) -> Result<PurgeResponse, ApiError> {
        let purged = self.shared.queue.delete_queue(name, force)?;
        Ok(PurgeResponse { purged })
    }
}
