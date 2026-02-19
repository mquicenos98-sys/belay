//! The periodic duties, and the threads that run them.
//!
//! | duty | thread | cadence |
//! |---|---|---|
//! | queue tick: promotions, lease expiry, retention, lost workers | `belay-queue-tick` | `engine.tick` |
//! | scheduler tick: firing due schedules | `belay-scheduler` | until the next fire time, at most `scheduler.tick` |
//! | workflow chores: retention of finished runs, reconciling running steps | `belay-workflows` | `workflows.sweep_every` |
//! | housekeeping: lease latencies into metrics, time-based checkpoints | `belay-housekeeping` | `metrics.refresh` |
//!
//! [`Engine::tick`] runs one round of all of them at a given time instead,
//! which is how tests with a manual clock drive an engine built without
//! background threads.

use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use belay_common::sync::{lock, spawn_periodic};
use belay_common::{debug, error, warn, Shutdown, Timestamp};
use belay_store::{CheckpointInfo, Kv};

use super::{Engine, Shared};
use crate::error::{Error, Result};

/// What one [`Engine::tick`] did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TickReport {
    /// The queue tick.
    pub queue: belay_queue::TickReport,
    /// The scheduler tick; empty when schedules may not fire here.
    pub schedules: belay_scheduler::TickReport,
    /// Workflow runs changed by reconciling their steps with the queue.
    pub workflows_recovered: usize,
    /// Finished workflow runs deleted for retention.
    pub workflows_deleted: usize,
    /// The checkpoint written, if one was due.
    pub checkpoint: Option<CheckpointInfo>,
    /// Duties that failed, one line each. They are retried next round.
    pub errors: Vec<String>,
}
