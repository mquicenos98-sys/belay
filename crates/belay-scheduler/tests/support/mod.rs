//! Helpers shared by the integration tests: a scheduler or workflow engine
//! wired to a manual clock, an in-memory dispatcher and a collecting event
//! sink, plus a store wrapper that can refuse writes to simulate a crash
//! before bookkeeping is saved.
// Each test binary uses a different subset of these helpers.
#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use belay_common::log::{set_max_level, LevelFilter};
use belay_common::{Clock, ManualClock, Timestamp};
use belay_json::Value;
use belay_proto::source::CollectingSink;
use belay_proto::workflow::{StepSpec, WorkflowSpec};
use belay_proto::{Job, JobSpec, ScheduleId, ScheduleSpec, Workflow, WorkflowId};
use belay_scheduler::{
    MemoryDispatcher, Scheduler, SchedulerOptions, TickReport, WorkflowEngine, WorkflowOptions,
};
use belay_store::{CheckpointInfo, KeyRange, Kv, KvStats, MemKv, StoreError, WriteBatch};

/// Keeps expected warnings (rejected dispatches, failing stores) out of
/// the test output.
pub fn quiet() {
    set_max_level(LevelFilter::Off);
}

pub fn ts(s: &str) -> Timestamp {
    s.parse().expect("test timestamps are valid RFC 3339")
}

pub fn id(s: &str) -> ScheduleId {
    s.parse().expect("test schedule ids are valid")
}

/// A schedule firing `work` jobs on queue `jobs`.
pub fn spec(name: &str, cron: &str) -> ScheduleSpec {
    ScheduleSpec::new(id(name), cron, JobSpec::new("jobs", "work"))
}

/// Fire times of a report, as RFC 3339 text.
pub fn fired_times(report: &TickReport) -> Vec<String> {
    report
        .fired
        .iter()
        .map(|f| f.fire_time.to_string())
        .collect()
}

/// A store that can be told to refuse writes, as if the process died just
/// before saving.
#[derive(Debug)]
pub struct FlakyKv {
    inner: Arc<dyn Kv>,
    refuse: AtomicBool,
    /// Writes still allowed before refusing; negative for no limit.
    budget: AtomicI64,
}
