//! JSON for events: one flat object per event, `type` naming the kind.

use belay_json::{FromJson, FromJsonError, ObjectBuilder, ObjectReader, ToJson, Value};

use super::{Event, EventKind};
use crate::wire::{BuilderExt, ReaderExt};

impl EventKind {
    /// Appends `type` and the kind's fields to `b`.
    fn write(&self, b: ObjectBuilder) -> ObjectBuilder {
        use EventKind::*;
        let b = b.field("type", self.type_name());
        match self {
            JobEnqueued {
                job,
                queue,
                kind,
                run_at,
            } => b
                .field("job", job)
                .field("queue", queue)
                .field("kind", kind)
                .timestamp("run_at", *run_at),
            JobStarted {
                job,
                queue,
                worker,
                attempt,
            }
            | JobExpired {
                job,
                queue,
                worker,
                attempt,
            } => b
                .field("job", job)
                .field("queue", queue)
                .field("worker", worker)
                .field("attempt", attempt),
            JobProgress {
                job,
                queue,
                progress,
            } => b
                .field("job", job)
                .field("queue", queue)
                .field("progress", progress),
            JobSucceeded {
                job,
                queue,
                attempt,
                runtime,
            } => b
                .field("job", job)
                .field("queue", queue)
                .field("attempt", attempt)
                .duration("runtime", *runtime),
            JobFailed {
                job,
                queue,
                attempt,
                error,
                retry_at,
            } => b
                .field("job", job)
                .field("queue", queue)
                .field("attempt", attempt)
                .field("error", error)
                .timestamp("retry_at", *retry_at),
            JobDead {
                job,
                queue,
                attempt,
                error,
            } => b
                .field("job", job)
                .field("queue", queue)
                .field("attempt", attempt)
                .field("error", error),
            JobCancelled { job, queue }
            | JobRetried { job, queue }
            | JobPromoted { job, queue }
            | JobDeleted { job, queue } => b.field("job", job).field("queue", queue),
            ScheduleCreated { schedule }
            | ScheduleUpdated { schedule }
            | ScheduleDeleted { schedule }
            | SchedulePaused { schedule }
            | ScheduleResumed { schedule } => b.field("schedule", schedule),
            ScheduleFired {
                schedule,
                job,
                fire_time,
            } => b
                .field("schedule", schedule)
                .field("job", job)
                .timestamp("fire_time", *fire_time),
            ScheduleMisfired {
                schedule,
                fire_time,
                missed,
                policy,
            } => b
                .field("schedule", schedule)
                .timestamp("fire_time", *fire_time)
                .field("missed", missed)
                .field("policy", policy),
            ScheduleSkipped {
                schedule,
                fire_time,
                reason,
            } => b
                .field("schedule", schedule)
                .timestamp("fire_time", *fire_time)
                .field("reason", reason),
            WorkflowStarted { workflow, name } => b.field("workflow", workflow).field("name", name),
            WorkflowStep {
                workflow,
                step,
                state,
                job,
            } => b
                .field("workflow", workflow)
                .field("step", step)
                .field("state", state)
                .opt_field("job", job),
            WorkflowFinished {
                workflow,
                name,
                state,
            } => b
                .field("workflow", workflow)
                .field("name", name)
                .field("state", state),
            QueuePaused { queue }
            | QueueResumed { queue }
            | QueueConfigured { queue }
            | QueueDeleted { queue } => b.field("queue", queue),
            QueuePurged {
                queue,
                state,
                count,
            } => b
                .field("queue", queue)
                .opt_field("state", state)
                .field("count", count),
            WorkerSeen { worker, queues } => b.field("worker", worker).field("queues", queues),
            WorkerLost { worker, last_seen } => {
                b.field("worker", worker).timestamp("last_seen", *last_seen)
            }
        }
    }

    /// Reads `type` and the kind's fields; other fields are left alone so
    /// the caller decides about unknown ones.
    fn read(r: &ObjectReader<'_>) -> Result<EventKind, FromJsonError> {
        use EventKind::*;
        let ty = r.req_str("type")?;
        Ok(match ty {
            "job.enqueued" => JobEnqueued {
                job: r.req("job")?,
                queue: r.req("queue")?,
                kind: r.req("kind")?,
                run_at: r.req_timestamp("run_at")?,
            },
            "job.started" => JobStarted {
                job: r.req("job")?,
                queue: r.req("queue")?,
                worker: r.req("worker")?,
                attempt: r.req("attempt")?,
            },
            "job.progress" => JobProgress {
                job: r.req("job")?,
                queue: r.req("queue")?,
                progress: r.req("progress")?,
            },
            "job.succeeded" => JobSucceeded {
                job: r.req("job")?,
                queue: r.req("queue")?,
                attempt: r.req("attempt")?,
                runtime: r.req_duration("runtime")?,
            },
            "job.failed" => JobFailed {
                job: r.req("job")?,
                queue: r.req("queue")?,
                attempt: r.req("attempt")?,
                error: r.req("error")?,
                retry_at: r.req_timestamp("retry_at")?,
            },
            "job.dead" => JobDead {
                job: r.req("job")?,
                queue: r.req("queue")?,
                attempt: r.req("attempt")?,
                error: r.req("error")?,
            },
            "job.cancelled" => JobCancelled {
                job: r.req("job")?,
                queue: r.req("queue")?,
            },
            "job.expired" => JobExpired {
                job: r.req("job")?,
                queue: r.req("queue")?,
                worker: r.req("worker")?,
                attempt: r.req("attempt")?,
            },
            "job.retried" => JobRetried {
                job: r.req("job")?,
                queue: r.req("queue")?,
            },
            "job.promoted" => JobPromoted {
                job: r.req("job")?,
                queue: r.req("queue")?,
            },
            "job.deleted" => JobDeleted {
                job: r.req("job")?,
                queue: r.req("queue")?,
            },
            "schedule.created" => ScheduleCreated {
                schedule: r.req("schedule")?,
            },
            "schedule.updated" => ScheduleUpdated {
                schedule: r.req("schedule")?,
            },
            "schedule.deleted" => ScheduleDeleted {
                schedule: r.req("schedule")?,
            },
            "schedule.paused" => SchedulePaused {
                schedule: r.req("schedule")?,
            },
            "schedule.resumed" => ScheduleResumed {
                schedule: r.req("schedule")?,
            },
            "schedule.fired" => ScheduleFired {
                schedule: r.req("schedule")?,
                job: r.req("job")?,
                fire_time: r.req_timestamp("fire_time")?,
            },
            "schedule.misfired" => ScheduleMisfired {
                schedule: r.req("schedule")?,
                fire_time: r.req_timestamp("fire_time")?,
                missed: r.req("missed")?,
                policy: r.req("policy")?,
            },
            "schedule.skipped" => ScheduleSkipped {
                schedule: r.req("schedule")?,
                fire_time: r.req_timestamp("fire_time")?,
                reason: r.req("reason")?,
            },
            "workflow.started" => WorkflowStarted {
                workflow: r.req("workflow")?,
                name: r.req("name")?,
            },
            "workflow.step" => WorkflowStep {
                workflow: r.req("workflow")?,
                step: r.req("step")?,
                state: r.req("state")?,
                job: r.opt("job")?,
            },
            "workflow.finished" => WorkflowFinished {
                workflow: r.req("workflow")?,
                name: r.req("name")?,
                state: r.req("state")?,
            },
            "queue.paused" => QueuePaused {
                queue: r.req("queue")?,
            },
            "queue.resumed" => QueueResumed {
                queue: r.req("queue")?,
            },
            "queue.purged" => QueuePurged {
                queue: r.req("queue")?,
                state: r.opt("state")?,
                count: r.req("count")?,
            },
            "queue.configured" => QueueConfigured {
                queue: r.req("queue")?,
            },
            "queue.deleted" => QueueDeleted {
                queue: r.req("queue")?,
            },
            "worker.seen" => WorkerSeen {
                worker: r.req("worker")?,
                queues: r.or_default("queues")?,
            },
            "worker.lost" => WorkerLost {
                worker: r.req("worker")?,
                last_seen: r.req_timestamp("last_seen")?,
            },
            other => {
                let e = FromJsonError::unknown_variant(other, "event type", EventKind::TYPES);
                return Err(r.invalid("type", e.message()));
            }
        })
    }
}

/// `{"type": ..., <fields>}`, without `seq` and `at`; the form engines and
/// tests use for a bare kind.
impl ToJson for EventKind {
    fn to_json(&self) -> Value {
        self.write(ObjectBuilder::new()).build()
    }
}

impl FromJson for EventKind {
    fn from_json(value: &Value) -> Result<EventKind, FromJsonError> {
        EventKind::read(&ObjectReader::new(value, "EventKind")?)
    }
}

impl ToJson for Event {
    fn to_json(&self) -> Value {
        let b = ObjectBuilder::new()
            .field("seq", self.seq)
            .timestamp("at", self.at);
        self.kind.write(b).build()
    }
}

impl FromJson for Event {
    fn from_json(value: &Value) -> Result<Event, FromJsonError> {
        let r = ObjectReader::new(value, "Event")?;
        Ok(Event {
            seq: r.req("seq")?,
            at: r.req_timestamp("at")?,
            kind: EventKind::read(&r)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::samples;
    use super::*;
    use belay_common::Timestamp;
    use belay_json::json;

    fn event(seq: u64, kind: EventKind) -> Event {
        Event {
            seq,
            at: Timestamp::from_millis(1_741_064_767_089),
            kind,
        }
    }

    #[test]
    fn every_kind_round_trips_as_an_event() {
        for (i, kind) in samples::all().into_iter().enumerate() {
            let e = event(i as u64 + 1, kind);
            let text = belay_json::to_string_of(&e);
            let back: Event =
                belay_json::from_str(&text).unwrap_or_else(|err| panic!("{text}: {err}"));
            assert_eq!(back, e);
            assert_eq!(EventKind::from_json(&e.kind.to_json()), Ok(e.kind.clone()));
        }
    }

    #[test]
    fn events_are_flat_objects_with_type_after_seq_and_at() {
        let kinds = samples::all();
        let e = event(42, kinds[3].clone());
        assert_eq!(
            belay_json::to_string_of(&e),
            concat!(
                r#"{"seq":42,"at":"2025-03-04T05:06:07.089Z","type":"job.succeeded","#,
                r#""job":"01HQ3K9Z8N6V2T4R5S7W0X1Y2Z","queue":"emails","attempt":1,"runtime":"1s250ms"}"#
            )
        );
        let purged = event(7, kinds[24].clone()).to_json();
        assert_eq!(purged["state"], "dead");
        assert_eq!(purged["count"], 12);
    }

    #[test]
    fn decoding_tolerates_new_fields_but_not_new_types() {
        let v = json!({
            "seq": 1, "at": "2025-01-01T00:00:00Z", "type": "queue.paused",
            "queue": "emails", "by": "ops"
        });
        let e = Event::from_json(&v).unwrap();
        assert_eq!(e.type_name(), "queue.paused");
        let v =
            json!({"seq": 1, "at": "2025-01-01T00:00:00Z", "type": "queue.pauzed", "queue": "q"});
        assert_eq!(
            Event::from_json(&v).unwrap_err().to_string(),
            r#"$.type: unknown event type "queue.pauzed", did you mean "queue.paused"?"#
        );
    }

    #[test]
    fn missing_fields_are_reported_against_the_event() {
        let v = json!({"seq": 1, "at": "2025-01-01T00:00:00Z", "type": "job.started", "job": "01HQ3K9Z8N6V2T4R5S7W0X1Y2Z", "queue": "q", "attempt": 1});
        assert_eq!(
            Event::from_json(&v).unwrap_err().to_string(),
            r#"$: missing required field "worker" in Event"#
        );
        let v =
            json!({"seq": -1, "at": "2025-01-01T00:00:00Z", "type": "queue.paused", "queue": "q"});
        assert_eq!(
            Event::from_json(&v).unwrap_err().to_string(),
            "$.seq: expected integer in range 0..=18446744073709551615, found -1"
        );
    }
}
