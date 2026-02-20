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

impl Engine {
    /// Runs one round of every periodic duty at `now`: the queue tick, the
    /// scheduler tick (when schedules may fire on this node), the workflow
    /// chores, the metrics refresh, and a checkpoint if
    /// `storage.checkpoint_interval` has passed since the last one.
    ///
    /// Engines built with background threads run these on their own; this
    /// is for engines built with
    /// [`background(false)`](super::EngineBuilder::background).
    pub fn tick(&self, now: Timestamp) -> TickReport {
        let shared = &self.shared;
        let mut report = TickReport::default();
        match shared.queue.tick(now) {
            Ok(r) => report.queue = r,
            Err(e) => report.errors.push(format!("queue tick: {e}")),
        }
        if let Some(r) = shared.run_schedules(now) {
            report.schedules = r;
        }
        let (recovered, deleted, errors) = shared.workflow_chores(now);
        report.workflows_recovered = recovered;
        report.workflows_deleted = deleted;
        report.errors.extend(errors);
        shared.refresh_metrics();
        match shared.checkpoint_if_due(now) {
            Ok(info) => report.checkpoint = info,
            Err(e) => report.errors.push(format!("checkpoint: {e}")),
        }
        report
    }

    /// Starts the background threads.
    pub(super) fn spawn_duties(&self) -> Result<()> {
        let config = &self.shared.config;
        let mut threads = lock(&self.threads);

        let shared = Arc::clone(&self.shared);
        threads.push(spawn(
            "belay-queue-tick",
            config.engine.tick,
            &self.shutdown,
            move || {
                let now = shared.clock.now();
                if let Err(e) = shared.queue.tick(now) {
                    warn!("queue tick failed"; error = %e);
                }
            },
        )?);

        if config.scheduler.enabled {
            let shared = Arc::clone(&self.shared);
            let shutdown = self.shutdown.clone();
            let handle = thread::Builder::new()
                .name("belay-scheduler".to_owned())
                .spawn(move || scheduler_loop(&shared, &shutdown))
                .map_err(|e| Error::io("starting the scheduler thread", e))?;
            threads.push(handle);
        }

        let shared = Arc::clone(&self.shared);
        threads.push(spawn(
            "belay-workflows",
            config.workflows.sweep_every,
            &self.shutdown,
            move || {
                let now = shared.clock.now();
                for problem in shared.workflow_chores(now).2 {
                    warn!("workflow chore failed"; error = %problem);
                }
            },
        )?);

        let shared = Arc::clone(&self.shared);
        threads.push(spawn(
            "belay-housekeeping",
            config.metrics.refresh,
            &self.shutdown,
            move || {
                shared.refresh_metrics();
                if let Err(e) = shared.checkpoint_if_due(shared.clock.now()) {
                    warn!("periodic checkpoint failed"; error = %e);
                }
            },
        )?);
        Ok(())
    }
}
