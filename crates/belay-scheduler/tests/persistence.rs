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
