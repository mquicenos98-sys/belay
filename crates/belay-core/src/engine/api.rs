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

/// Schedules and calendars.
impl Engine {
    /// One page of schedules, by id.
    pub fn list_schedules(&self, paging: &Paging) -> Result<Page<Schedule>, ApiError> {
        let limit = paging.effective_limit()?;
        // The scheduler would treat a job cursor as "from the start";
        // refuse it instead, like the other name-ordered listings.
        paging.after_name()?;
        Ok(self
            .shared
            .scheduler
            .list_page(paging.cursor.as_ref(), limit))
    }

    /// The schedule `id`.
    pub fn get_schedule(&self, id: &ScheduleId) -> Result<Schedule, ApiError> {
        self.shared
            .scheduler
            .get(id)
            .ok_or_else(|| ApiError::not_found("schedule", id))
    }

    /// Creates a schedule; `already_exists` if the id is taken.
    pub fn create_schedule(&self, spec: ScheduleSpec) -> Result<Schedule, ApiError> {
        Ok(self.shared.scheduler.create(spec)?)
    }

    /// Replaces an existing schedule's definition.
    pub fn update_schedule(&self, spec: ScheduleSpec) -> Result<Schedule, ApiError> {
        Ok(self.shared.scheduler.update(spec)?)
    }

    /// Creates or replaces a schedule (`PUT /v1/schedules/:id`).
    pub fn put_schedule(&self, spec: ScheduleSpec) -> Result<(Schedule, Upserted), ApiError> {
        Ok(self.shared.scheduler.upsert(spec)?)
    }

    /// Deletes a schedule, returning its last state. Jobs it created stay.
    pub fn delete_schedule(&self, id: &ScheduleId) -> Result<Schedule, ApiError> {
        Ok(self.shared.scheduler.delete(id)?)
    }

    /// Pauses a schedule.
    pub fn pause_schedule(&self, id: &ScheduleId) -> Result<Schedule, ApiError> {
        Ok(self.shared.scheduler.pause(id)?)
    }

    /// Resumes a schedule; fires missed while paused are not caught up.
    pub fn resume_schedule(&self, id: &ScheduleId) -> Result<Schedule, ApiError> {
        Ok(self.shared.scheduler.resume(id)?)
    }

    /// Fires a schedule now, whatever its cron expression and state say.
    pub fn trigger_schedule(&self, id: &ScheduleId) -> Result<TriggerResponse, ApiError> {
        let fired = self.shared.scheduler.trigger_now(id)?;
        Ok(TriggerResponse { job: fired.job })
    }

    /// The next `n` fire times of a schedule.
    pub fn preview_schedule(&self, id: &ScheduleId, n: usize) -> Result<PreviewResponse, ApiError> {
        let times = self.shared.scheduler.preview(id, n)?;
        Ok(PreviewResponse { times })
    }

    /// The first `n` fire times of a schedule that is not stored, from now.
    pub fn preview_spec(&self, spec: &ScheduleSpec, n: usize) -> Result<PreviewResponse, ApiError> {
        let now = self.shared.clock.now();
        let times = self.shared.scheduler.preview_spec(spec, now, n)?;
        Ok(PreviewResponse { times })
    }

    /// One page of calendars, by name.
    pub fn list_calendars(&self, paging: &Paging) -> Result<Page<CalendarSpec>, ApiError> {
        let calendars = self.shared.scheduler.list_calendars();
        page_by_name(calendars, |c| c.name.as_str(), paging)
    }

    /// The calendar `name`.
    pub fn get_calendar(&self, name: &str) -> Result<CalendarSpec, ApiError> {
        self.shared
            .scheduler
            .get_calendar(name)
            .ok_or_else(|| ApiError::not_found("calendar", name))
    }

    /// Creates or replaces a calendar; schedules using it are rescheduled.
    pub fn put_calendar(&self, spec: CalendarSpec) -> Result<(CalendarSpec, Upserted), ApiError> {
        let upserted = self.shared.scheduler.put_calendar(spec.clone())?;
        Ok((spec, upserted))
    }

    /// Deletes a calendar no schedule uses.
    pub fn delete_calendar(&self, name: &str) -> Result<CalendarSpec, ApiError> {
        Ok(self.shared.scheduler.delete_calendar(name)?)
    }
}

/// Workflows.
impl Engine {
    /// Starts a workflow run.
    pub fn submit_workflow(&self, spec: WorkflowSpec) -> Result<Workflow, ApiError> {
        Ok(self.shared.workflows.call(|w| w.submit(spec))?)
    }

    /// The run `id`.
    pub fn get_workflow(&self, id: WorkflowId) -> Result<Workflow, ApiError> {
        self.shared
            .workflows
            .engine()
            .get(id)?
            .ok_or_else(|| ApiError::not_found("workflow", id))
    }

    /// The run `id` with the failure message of each failed step.
    pub fn workflow_details(&self, id: WorkflowId) -> Result<WorkflowDetails, ApiError> {
        self.shared
            .workflows
            .engine()
            .details(id)?
            .ok_or_else(|| ApiError::not_found("workflow", id))
    }

    /// One page of runs matching `filter`.
    pub fn list_workflows(&self, filter: &WorkflowFilter) -> Result<Page<Workflow>, ApiError> {
        Ok(self.shared.workflows.engine().list(filter)?)
    }

    /// Cancels a run and its running step jobs.
    pub fn cancel_workflow(&self, id: WorkflowId) -> Result<Workflow, ApiError> {
        Ok(self.shared.workflows.call(|w| w.cancel(id))?)
    }
}

/// Workers, cluster and administration.
impl Engine {
    /// One page of workers seen recently, by id.
    pub fn workers(&self, paging: &Paging) -> Result<Page<WorkerInfo>, ApiError> {
        let workers = self.shared.queue.workers();
        page_by_name(workers, |w| w.id.as_str(), paging)
    }

    /// Whether this engine is part of a cluster.
    #[must_use]
    pub fn is_clustered(&self) -> bool {
        self.shared.cluster.is_some()
    }

    /// Which node should serve requests about `queue`.
    #[must_use]
    pub fn owner_of(&self, queue: &QueueName) -> Ownership {
        self.owner_of_key(&queue_key(queue))
    }

    /// Which node fires schedule `id` and should serve requests about it.
    #[must_use]
    pub fn owner_of_schedule(&self, id: &ScheduleId) -> Ownership {
        self.owner_of_key(&schedule_key(id))
    }

    /// The owner of an arbitrary ownership key.
    #[must_use]
    pub fn owner_of_key(&self, key: &str) -> Ownership {
        match &self.shared.cluster {
            Some(cluster) => cluster.ownership(key),
            None => Ownership::Local,
        }
    }

    /// The cluster members as this node sees them, by id; just this node
    /// when not clustered.
    #[must_use]
    pub fn members(&self) -> Vec<MemberInfo> {
        match &self.shared.cluster {
            Some(cluster) => cluster.members(),
            None => vec![MemberInfo {
                id: self.shared.node_id.clone(),
                addr: self
                    .shared
                    .config
                    .cluster
                    .api_url(&self.shared.config.server),
                state: MemberState::Alive,
                leader: true,
            }],
        }
    }

    /// Whether the cluster node has joined through a seed (always true
    /// when not clustered).
    #[must_use]
    pub fn is_joined(&self) -> bool {
        self.shared.cluster.as_ref().map_or(true, |c| c.is_joined())
    }

    /// Writes a checkpoint of the store now.
    pub fn checkpoint(&self) -> Result<CheckpointResponse, ApiError> {
        let started = Instant::now();
        let info = self
            .shared
            .kv
            .checkpoint()
            .map_err(|e| ApiError::from(belay_queue::Error::Store(e)))?;
        *belay_common::sync::lock(&self.shared.last_checkpoint) = self.shared.clock.now();
        Ok(CheckpointResponse {
            seq: info.seq,
            took: started.elapsed(),
        })
    }

    /// What the store is and its statistics.
    #[must_use]
    pub fn store_report(&self) -> StoreReport {
        let shared = &self.shared;
        StoreReport {
            storage: shared.storage.as_str().to_owned(),
            dir: shared.log_kv.as_ref().map(|kv| kv.dir().to_path_buf()),
            stats: shared.kv.stats(),
        }
    }

    /// Rebuilds the queue's indexes from the store and compares them with
    /// the live ones. Holds the queue lock for a full scan: a diagnostic,
    /// not something to run periodically.
    pub fn verify(&self) -> Result<VerifyOutcome, ApiError> {
        Ok(self.shared.queue.verify()?.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Named(&'static str);

    fn names(page: &Page<Named>) -> Vec<&'static str> {
        page.items.iter().map(|n| n.0).collect()
    }

    #[test]
    fn name_pages_are_sorted_and_continue_after_the_cursor() {
        let items = || vec![Named("c"), Named("a"), Named("d"), Named("b")];
        let first = page_by_name(items(), |n| n.0, &Paging::new().limit(3)).unwrap();
        assert_eq!(names(&first), ["a", "b", "c"]);
        let cursor = first.next_cursor.clone().expect("more to come");
        let rest = page_by_name(items(), |n| n.0, &Paging::new().after(cursor).limit(3)).unwrap();
        assert_eq!(names(&rest), ["d"]);
        assert!(rest.next_cursor.is_none());
    }

    #[test]
    fn foreign_cursors_and_bad_limits_are_invalid_arguments() {
        let job_cursor = Cursor::after_job(JobId::generate());
        let err =
            page_by_name(vec![Named("a")], |n| n.0, &Paging::new().after(job_cursor)).unwrap_err();
        assert_eq!(err.message, "$.cursor: was not issued by this listing");
        let err = page_by_name(vec![Named("a")], |n| n.0, &Paging::new().limit(5000)).unwrap_err();
        assert_eq!(err.message, "$.limit: must be between 1 and 1000, got 5000");
        assert!(Paging::from_pairs([("limit", "ten")]).is_err());
        assert_eq!(Paging::from_pairs([("cursor", "")]).unwrap(), Paging::new());
    }

    #[test]
    fn verify_outcomes_render_as_json() {
        let outcome = VerifyOutcome {
            jobs: 3,
            queues: 1,
            claims: 0,
            problems: vec!["job x: state differs".into()],
            more_problems: 0,
        };
        let json = outcome.to_json();
        assert_eq!(json["consistent"], false);
        assert_eq!(json["problems"][0], "job x: state differs");
    }
}
