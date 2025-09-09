//! Durability: reopening both engines on a log-structured store over an
//! in-memory file system (including after a simulated power loss), and
//! workflow recovery after completions or bookkeeping were lost.

mod support;

use std::sync::Arc;
use std::time::Duration;

use belay_common::{Clock, ManualClock};
use belay_json::{json, Value};
use belay_proto::source::CollectingSink;
use belay_proto::workflow::{StepState, WorkflowState};
use belay_proto::{Job, JobId, JobSpec, JobState, MisfirePolicy};
use belay_scheduler::{
    DispatchError, DispatchOutcome, Dispatcher, MemoryDispatcher, Scheduler, WorkflowEngine,
};
use belay_store::vfs::{Crash, MemFs};
use belay_store::{Kv, LogKv, MemKv, Options, SyncPolicy};

use support::{id, quiet, spec, step, ts, workflow, FlakyKv, Flow};

fn open_log(fs: &MemFs) -> Arc<dyn Kv> {
    quiet();
    Arc::new(
        LogKv::open(
            "store",
            Options::new()
                .fs(fs.clone())
                .sync(SyncPolicy::Always)
                .manual_checkpoints(),
        )
        .expect("opening the store"),
    )
}

struct Parts {
    clock: ManualClock,
    queue: Arc<MemoryDispatcher>,
    sink: Arc<CollectingSink>,
}

impl Parts {
    fn new(start: &str) -> Parts {
        let clock = ManualClock::new(ts(start));
        Parts {
            queue: Arc::new(MemoryDispatcher::new(clock.shared())),
            sink: Arc::new(CollectingSink::new()),
            clock,
        }
    }

    fn scheduler(&self, kv: Arc<dyn Kv>) -> Scheduler {
        Scheduler::open(
            kv,
            self.clock.shared(),
            self.queue.clone(),
            self.sink.clone(),
        )
        .expect("opening the scheduler")
    }

    fn engine(&self, kv: Arc<dyn Kv>) -> WorkflowEngine {
        WorkflowEngine::open(
            kv,
            self.clock.shared(),
            self.queue.clone(),
            self.sink.clone(),
        )
        .expect("opening the workflow engine")
    }
}

#[test]
fn schedules_and_calendars_survive_reopening() {
    let fs = MemFs::new();
    let parts = Parts::new("2025-06-02T10:07:00Z");
    {
        let scheduler = parts.scheduler(open_log(&fs));
        scheduler
            .put_calendar(
                belay_json::from_str(r#"{"name": "weekdays", "exclude_weekdays": ["sat", "sun"]}"#)
                    .unwrap(),
            )
            .unwrap();
        scheduler
            .create(spec("sync", "*/15 * * * *").calendar("weekdays"))
            .unwrap();
        scheduler
            .create(spec("nightly", "0 2 * * *").misfire(MisfirePolicy::Skip))
            .unwrap();
        scheduler.pause(&id("nightly")).unwrap();
        parts.clock.set(ts("2025-06-02T10:15:00Z"));
        assert_eq!(scheduler.tick(parts.clock.now()).fired.len(), 1);
    }
    let before = parts.queue.job_count();
    let scheduler = parts.scheduler(open_log(&fs));
    let sync = scheduler.get(&id("sync")).unwrap();
    assert_eq!(sync.fire_count, 1);
    assert_eq!(sync.next_fire_at, Some(ts("2025-06-02T10:30:00Z")));
    assert!(scheduler.get(&id("nightly")).unwrap().spec.paused);
    assert!(scheduler.get_calendar("weekdays").is_some());
    // It carries on where it stopped.
    parts.clock.set(ts("2025-06-02T10:30:00Z"));
    assert_eq!(scheduler.tick(parts.clock.now()).fired.len(), 1);
    assert_eq!(parts.queue.job_count(), before + 1);
}

#[test]
fn a_power_loss_keeps_every_saved_fire() {
    let fs = MemFs::new();
    let parts = Parts::new("2025-06-02T10:07:00Z");
    let scheduler = parts.scheduler(open_log(&fs));
    scheduler.create(spec("sync", "*/15 * * * *")).unwrap();
    for t in ["2025-06-02T10:15:00Z", "2025-06-02T10:30:00Z"] {
        parts.clock.set(ts(t));
        assert_eq!(scheduler.tick(parts.clock.now()).fired.len(), 1);
    }
    // Pull the plug while the original store is still open.
    let crashed = fs.crash_copy(&Crash::PowerLoss);
    let recovered = parts.scheduler(open_log(&crashed));
    let sync = recovered.get(&id("sync")).unwrap();
    assert_eq!(sync.fire_count, 2);
    assert_eq!(sync.last_fire_at, Some(ts("2025-06-02T10:30:00Z")));
    assert_eq!(sync.next_fire_at, Some(ts("2025-06-02T10:45:00Z")));
}
