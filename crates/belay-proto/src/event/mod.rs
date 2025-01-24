//! The event stream: every notable change in the system, numbered.
//!
//! Engines report what happens through an [`EventSink`](crate::EventSink)
//! as bare [`EventKind`]s; the event bus in belay-core stamps each with a
//! sequence number and time, producing an [`Event`]. Subscribers
//! (`GET /v1/events`, server-sent events) resume from a sequence number.
//!
//! # Wire form
//!
//! One flat object: `seq`, `at`, `type`, then the fields of the kind.
//!
//! ```json
//! {"seq":42,"at":"2025-03-04T05:06:07.089Z","type":"job.succeeded",
//!  "job":"01HQ3K9Z8N6V2T4R5S7W0X1Y2Z","queue":"emails","attempt":1,"runtime":"1s250ms"}
//! ```
//!
//! Type names are `<category>.<what>` and stable; see [`EventKind::TYPES`].
//! Decoding ignores fields it does not know, so fields can be added to a
//! kind without breaking older clients; a new *type* is not readable by an
//! older client, which should skip events it cannot decode.

mod codec;

mod filter;

pub use filter::{EventFilter, EventPattern};

use std::time::Duration;

use belay_common::Timestamp;

use crate::ids::{JobId, QueueName, ScheduleId, WorkerId, WorkflowId};
use crate::job::{JobState, Progress};
use crate::schedule::MisfirePolicy;
use crate::wire::text_enum;
use crate::workflow::{StepState, WorkflowState};

/// An event as delivered to subscribers.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// Position in the stream, increasing by one per event.
    pub seq: u64,
    /// When it happened.
    pub at: Timestamp,
    /// What happened.
    pub kind: EventKind,
}

impl Event {
    /// The event's type name, e.g. `job.succeeded`.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        self.kind.type_name()
    }
}

text_enum! {
    /// Why a schedule did not fire at one of its times.
    SkipReason, "skip reason" {
        /// The previous job was still running under the `skip` overlap policy.
        Overlap = "overlap",
        /// The time fell outside the schedule's calendar.
        Calendar = "calendar",
    }
}

/// What happened. Field meanings are given on each variant; `job`, `queue`,
/// `schedule`, `workflow` and `worker` always name the entity concerned.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
#[allow(missing_docs)] // variant fields are described on each variant
pub enum EventKind {
    /// `job.enqueued`: a job was accepted; it becomes eligible at `run_at`.
    JobEnqueued {
        job: JobId,
        queue: QueueName,
        kind: String,
        run_at: Timestamp,
    },
    /// `job.started`: a worker leased the job for attempt number `attempt`.
    JobStarted {
        job: JobId,
        queue: QueueName,
        worker: WorkerId,
        attempt: u32,
    },
    /// `job.progress`: the running attempt reported progress.
    JobProgress {
        job: JobId,
        queue: QueueName,
        progress: Progress,
    },
    /// `job.succeeded`: attempt `attempt` completed after running for
    /// `runtime`.
    JobSucceeded {
        job: JobId,
        queue: QueueName,
        attempt: u32,
        runtime: Duration,
    },
    /// `job.failed`: attempt `attempt` failed with `error`; the job retries
    /// at `retry_at`.
    JobFailed {
        job: JobId,
        queue: QueueName,
        attempt: u32,
        error: String,
        retry_at: Timestamp,
    },
    /// `job.dead`: attempt `attempt` failed with `error` and no retry is
    /// left.
    JobDead {
        job: JobId,
        queue: QueueName,
        attempt: u32,
        error: String,
    },
    /// `job.cancelled`: the job was cancelled.
    JobCancelled { job: JobId, queue: QueueName },
    /// `job.expired`: `worker`'s lease on attempt `attempt` ran out; a
    /// `job.failed` or `job.dead` event follows.
    JobExpired {
        job: JobId,
        queue: QueueName,
        worker: WorkerId,
        attempt: u32,
    },
    /// `job.retried`: a dead or cancelled job was sent back to `ready`.
    JobRetried { job: JobId, queue: QueueName },
    /// `job.promoted`: a waiting job was made ready early.
    JobPromoted { job: JobId, queue: QueueName },
    /// `job.deleted`: a finished job was removed.
    JobDeleted { job: JobId, queue: QueueName },

    /// `schedule.created`.
    ScheduleCreated { schedule: ScheduleId },
    /// `schedule.updated`: its spec was replaced.
    ScheduleUpdated { schedule: ScheduleId },
    /// `schedule.deleted`.
    ScheduleDeleted { schedule: ScheduleId },
    /// `schedule.paused`.
    SchedulePaused { schedule: ScheduleId },
    /// `schedule.resumed`.
    ScheduleResumed { schedule: ScheduleId },
    /// `schedule.fired`: the fire at nominal time `fire_time` enqueued `job`.
    ScheduleFired {
        schedule: ScheduleId,
        job: JobId,
        fire_time: Timestamp,
    },
    /// `schedule.misfired`: `missed` fire times were missed, the first at
    /// `fire_time`; `policy` decided what was done about them.
    ScheduleMisfired {
        schedule: ScheduleId,
        fire_time: Timestamp,
        missed: u32,
        policy: MisfirePolicy,
    },
    /// `schedule.skipped`: the fire at `fire_time` was skipped for `reason`.
    ScheduleSkipped {
        schedule: ScheduleId,
        fire_time: Timestamp,
        reason: SkipReason,
    },

    /// `workflow.started`: run `workflow` of the workflow named `name`.
    WorkflowStarted { workflow: WorkflowId, name: String },
    /// `workflow.step`: step `step` moved to `state`; `job` once dispatched.
    WorkflowStep {
        workflow: WorkflowId,
        step: String,
        state: StepState,
        job: Option<JobId>,
    },
    /// `workflow.finished`: the run ended in `state`.
    WorkflowFinished {
        workflow: WorkflowId,
        name: String,
        state: WorkflowState,
    },

    /// `queue.paused`.
    QueuePaused { queue: QueueName },
    /// `queue.resumed`.
    QueueResumed { queue: QueueName },
    /// `queue.purged`: `count` jobs removed, only those in `state` if set.
    QueuePurged {
        queue: QueueName,
        state: Option<JobState>,
        count: u64,
    },
    /// `queue.configured`: its configuration was replaced.
    QueueConfigured { queue: QueueName },
    /// `queue.deleted`.
    QueueDeleted { queue: QueueName },

    /// `worker.seen`: a worker leased for the first time (or after being
    /// lost), serving `queues`.
    WorkerSeen {
        worker: WorkerId,
        queues: Vec<QueueName>,
    },
    /// `worker.lost`: nothing heard from `worker` since `last_seen`.
    WorkerLost {
        worker: WorkerId,
        last_seen: Timestamp,
    },
}
