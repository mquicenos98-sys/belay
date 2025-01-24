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

/// Event categories, the part of a type name before the dot.
pub const CATEGORIES: &[&str] = &["job", "schedule", "workflow", "queue", "worker"];

impl EventKind {
    /// Every type name, grouped by category.
    pub const TYPES: &'static [&'static str] = &[
        "job.enqueued",
        "job.started",
        "job.progress",
        "job.succeeded",
        "job.failed",
        "job.dead",
        "job.cancelled",
        "job.expired",
        "job.retried",
        "job.promoted",
        "job.deleted",
        "schedule.created",
        "schedule.updated",
        "schedule.deleted",
        "schedule.paused",
        "schedule.resumed",
        "schedule.fired",
        "schedule.misfired",
        "schedule.skipped",
        "workflow.started",
        "workflow.step",
        "workflow.finished",
        "queue.paused",
        "queue.resumed",
        "queue.purged",
        "queue.configured",
        "queue.deleted",
        "worker.seen",
        "worker.lost",
    ];

    /// The stable type name, e.g. `job.succeeded`.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        use EventKind::*;
        match self {
            JobEnqueued { .. } => "job.enqueued",
            JobStarted { .. } => "job.started",
            JobProgress { .. } => "job.progress",
            JobSucceeded { .. } => "job.succeeded",
            JobFailed { .. } => "job.failed",
            JobDead { .. } => "job.dead",
            JobCancelled { .. } => "job.cancelled",
            JobExpired { .. } => "job.expired",
            JobRetried { .. } => "job.retried",
            JobPromoted { .. } => "job.promoted",
            JobDeleted { .. } => "job.deleted",
            ScheduleCreated { .. } => "schedule.created",
            ScheduleUpdated { .. } => "schedule.updated",
            ScheduleDeleted { .. } => "schedule.deleted",
            SchedulePaused { .. } => "schedule.paused",
            ScheduleResumed { .. } => "schedule.resumed",
            ScheduleFired { .. } => "schedule.fired",
            ScheduleMisfired { .. } => "schedule.misfired",
            ScheduleSkipped { .. } => "schedule.skipped",
            WorkflowStarted { .. } => "workflow.started",
            WorkflowStep { .. } => "workflow.step",
            WorkflowFinished { .. } => "workflow.finished",
            QueuePaused { .. } => "queue.paused",
            QueueResumed { .. } => "queue.resumed",
            QueuePurged { .. } => "queue.purged",
            QueueConfigured { .. } => "queue.configured",
            QueueDeleted { .. } => "queue.deleted",
            WorkerSeen { .. } => "worker.seen",
            WorkerLost { .. } => "worker.lost",
        }
    }

    /// The category: `job`, `schedule`, `workflow`, `queue` or `worker`.
    #[must_use]
    pub fn category(&self) -> &'static str {
        let name = self.type_name();
        name.split_once('.').map_or(name, |(category, _)| category)
    }

    /// The job concerned, if any.
    #[must_use]
    pub fn job(&self) -> Option<JobId> {
        use EventKind::*;
        match self {
            JobEnqueued { job, .. }
            | JobStarted { job, .. }
            | JobProgress { job, .. }
            | JobSucceeded { job, .. }
            | JobFailed { job, .. }
            | JobDead { job, .. }
            | JobCancelled { job, .. }
            | JobExpired { job, .. }
            | JobRetried { job, .. }
            | JobPromoted { job, .. }
            | JobDeleted { job, .. }
            | ScheduleFired { job, .. } => Some(*job),
            WorkflowStep { job, .. } => *job,
            _ => None,
        }
    }

    /// The queue concerned, if any.
    #[must_use]
    pub fn queue(&self) -> Option<&QueueName> {
        use EventKind::*;
        match self {
            JobEnqueued { queue, .. }
            | JobStarted { queue, .. }
            | JobProgress { queue, .. }
            | JobSucceeded { queue, .. }
            | JobFailed { queue, .. }
            | JobDead { queue, .. }
            | JobCancelled { queue, .. }
            | JobExpired { queue, .. }
            | JobRetried { queue, .. }
            | JobPromoted { queue, .. }
            | JobDeleted { queue, .. }
            | QueuePaused { queue }
            | QueueResumed { queue }
            | QueuePurged { queue, .. }
            | QueueConfigured { queue }
            | QueueDeleted { queue } => Some(queue),
            _ => None,
        }
    }

    /// The schedule concerned, if any.
    #[must_use]
    pub fn schedule(&self) -> Option<&ScheduleId> {
        use EventKind::*;
        match self {
            ScheduleCreated { schedule }
            | ScheduleUpdated { schedule }
            | ScheduleDeleted { schedule }
            | SchedulePaused { schedule }
            | ScheduleResumed { schedule }
            | ScheduleFired { schedule, .. }
            | ScheduleMisfired { schedule, .. }
            | ScheduleSkipped { schedule, .. } => Some(schedule),
            _ => None,
        }
    }

    /// The workflow run concerned, if any.
    #[must_use]
    pub fn workflow(&self) -> Option<WorkflowId> {
        use EventKind::*;
        match self {
            WorkflowStarted { workflow, .. }
            | WorkflowStep { workflow, .. }
            | WorkflowFinished { workflow, .. } => Some(*workflow),
            _ => None,
        }
    }

    /// The worker concerned, if any.
    #[must_use]
    pub fn worker(&self) -> Option<&WorkerId> {
        use EventKind::*;
        match self {
            JobStarted { worker, .. }
            | JobExpired { worker, .. }
            | WorkerSeen { worker, .. }
            | WorkerLost { worker, .. } => Some(worker),
            _ => None,
        }
    }
}

#[cfg(test)]
pub(crate) mod samples {
    //! One value of every kind, shared by the codec and filter tests.

    use super::*;

    pub(crate) fn all() -> Vec<EventKind> {
        let job: JobId = "01HQ3K9Z8N6V2T4R5S7W0X1Y2Z".parse().unwrap();
        let queue = QueueName::from_static("emails");
        let worker = WorkerId::from_static("web-1:4121");
        let schedule = ScheduleId::from_static("nightly");
        let workflow: WorkflowId = "01HQ3K9Z8N6V2T4R5S7W0X1Y30".parse().unwrap();
        let at = Timestamp::from_millis(1_741_064_767_089);
        let q = || queue.clone();
        use EventKind::*;
        vec![
            JobEnqueued {
                job,
                queue: q(),
                kind: "send".into(),
                run_at: at,
            },
            JobStarted {
                job,
                queue: q(),
                worker: worker.clone(),
                attempt: 1,
            },
            JobProgress {
                job,
                queue: q(),
                progress: Progress::percent(50.0).with_message("half"),
            },
            JobSucceeded {
                job,
                queue: q(),
                attempt: 1,
                runtime: Duration::from_millis(1_250),
            },
            JobFailed {
                job,
                queue: q(),
                attempt: 2,
                error: "smtp 451".into(),
                retry_at: at,
            },
            JobDead {
                job,
                queue: q(),
                attempt: 25,
                error: "bounced".into(),
            },
            JobCancelled { job, queue: q() },
            JobExpired {
                job,
                queue: q(),
                worker: worker.clone(),
                attempt: 3,
            },
            JobRetried { job, queue: q() },
            JobPromoted { job, queue: q() },
            JobDeleted { job, queue: q() },
            ScheduleCreated {
                schedule: schedule.clone(),
            },
            ScheduleUpdated {
                schedule: schedule.clone(),
            },
            ScheduleDeleted {
                schedule: schedule.clone(),
            },
            SchedulePaused {
                schedule: schedule.clone(),
            },
            ScheduleResumed {
                schedule: schedule.clone(),
            },
            ScheduleFired {
                schedule: schedule.clone(),
                job,
                fire_time: at,
            },
            ScheduleMisfired {
                schedule: schedule.clone(),
                fire_time: at,
                missed: 4,
                policy: MisfirePolicy::FireAll { max: 10 },
            },
            ScheduleSkipped {
                schedule,
                fire_time: at,
                reason: SkipReason::Overlap,
            },
            WorkflowStarted {
                workflow,
                name: "etl".into(),
            },
            WorkflowStep {
                workflow,
                step: "load".into(),
                state: StepState::Running,
                job: Some(job),
            },
            WorkflowFinished {
                workflow,
                name: "etl".into(),
                state: WorkflowState::Succeeded,
            },
            QueuePaused { queue: q() },
            QueueResumed { queue: q() },
            QueuePurged {
                queue: q(),
                state: Some(JobState::Dead),
                count: 12,
            },
            QueueConfigured { queue: q() },
            QueueDeleted { queue: q() },
            WorkerSeen {
                worker: worker.clone(),
                queues: vec![q()],
            },
            WorkerLost {
                worker,
                last_seen: at,
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_has_a_listed_unique_type_name() {
        let kinds = samples::all();
        let names: Vec<&str> = kinds.iter().map(EventKind::type_name).collect();
        assert_eq!(
            names,
            EventKind::TYPES,
            "samples cover every type, in order"
        );
        for name in EventKind::TYPES {
            let (category, what) = name.split_once('.').expect("dotted name");
            assert!(CATEGORIES.contains(&category), "{name}");
            assert!(what.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
        }
    }

    #[test]
    fn accessors_name_the_entities_concerned() {
        let kinds = samples::all();
        let started = &kinds[1];
        assert_eq!(started.category(), "job");
        assert_eq!(started.queue().map(QueueName::as_str), Some("emails"));
        assert_eq!(started.worker().map(WorkerId::as_str), Some("web-1:4121"));
        let fired = &kinds[16];
        assert_eq!(fired.schedule().map(ScheduleId::as_str), Some("nightly"));
        assert!(fired.job().is_some() && fired.queue().is_none());
        let step = &kinds[20];
        assert!(step.workflow().is_some() && step.job().is_some());
        for kind in &kinds {
            let has_queue = kind.queue().is_some();
            let expected = matches!(kind.category(), "job" | "queue");
            assert_eq!(has_queue, expected, "{}", kind.type_name());
        }
    }
}
