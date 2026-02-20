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

fn spawn(
    name: &str,
    period: Duration,
    shutdown: &Shutdown,
    mut f: impl FnMut() + Send + 'static,
) -> Result<thread::JoinHandle<()>> {
    spawn_periodic(name, period, shutdown.clone(), move |_| f())
        .map_err(|e| Error::io(format!("starting the {name} thread"), e))
}

/// Sleeps until the next fire time (at most `scheduler.tick`, which bounds
/// how late it notices schedules added or changed meanwhile), ticks, and
/// repeats until shutdown.
fn scheduler_loop(shared: &Shared, shutdown: &Shutdown) {
    let max_sleep = shared.config.scheduler.tick;
    while !shutdown.is_triggered() {
        let now = shared.clock.now();
        if shared.scheduler.next_wakeup().is_some_and(|t| t <= now) {
            let ran = panic::catch_unwind(AssertUnwindSafe(|| shared.run_schedules(now)));
            if ran.is_err() {
                error!("scheduler tick panicked");
            }
        }
        let now = shared.clock.now();
        // A wakeup still due after the tick is waiting for something else
        // (a retry delay, the cluster join); do not spin on it.
        let sleep = match shared.scheduler.next_wakeup() {
            Some(t) if t > now => t.saturating_duration_since(now).min(max_sleep),
            _ => max_sleep,
        };
        if shutdown.wait_timeout(sleep.max(Duration::from_millis(1))) {
            break;
        }
    }
}

impl Shared {
    /// Ticks the scheduler if schedules may fire on this node: always when
    /// not clustered; when clustered, once the node has joined (or its
    /// grace period has passed), firing only the schedules it owns.
    pub(crate) fn run_schedules(&self, now: Timestamp) -> Option<belay_scheduler::TickReport> {
        if !self.config.scheduler.enabled {
            return None;
        }
        let report = match &self.cluster {
            None => self.scheduler.tick(now),
            Some(cluster) if cluster.may_fire() => {
                self.scheduler.tick_owned(now, &|id| cluster.fires(id))
            }
            Some(_) => return None,
        };
        for problem in &report.errors {
            warn!("schedule fire failed"; error = %problem.message, schedule = ?problem.schedule);
        }
        if !report.is_empty() {
            debug!("scheduler tick"; fired = report.fired.len(), skipped = report.skipped.len());
        }
        Some(report)
    }

    /// Reconciles running workflow steps with their jobs and deletes runs
    /// past `workflows.keep_for`. Returns (runs changed, runs deleted,
    /// problems).
    pub(crate) fn workflow_chores(&self, now: Timestamp) -> (usize, usize, Vec<String>) {
        let mut problems = Vec::new();
        let recovered = self.workflows.call(|w| w.recover()).unwrap_or_else(|e| {
            problems.push(format!("workflow recovery: {e}"));
            0
        });
        let keep_for = self.config.workflows.keep_for;
        let deleted = self
            .workflows
            .call(|w| w.sweep(now, keep_for))
            .unwrap_or_else(|e| {
                problems.push(format!("workflow retention: {e}"));
                0
            });
        (recovered, deleted, problems)
    }

    /// Folds lease latency samples into the metrics.
    pub(crate) fn refresh_metrics(&self) {
        let samples = self.queue.drain_lease_latencies();
        self.metrics.observe_lease_waits(&samples);
    }

    /// Writes a checkpoint when `storage.checkpoint_interval` has passed
    /// since the last one. The store skips it if nothing was written.
    pub(crate) fn checkpoint_if_due(
        &self,
        now: Timestamp,
    ) -> belay_store::Result<Option<CheckpointInfo>> {
        let interval = self.config.storage.checkpoint_interval;
        if self.log_kv.is_none() || interval.is_zero() {
            return Ok(None);
        }
        {
            let mut last = lock(&self.last_checkpoint);
            let elapsed = now.saturating_duration_since(*last);
            *last = now;
            if elapsed < interval {
                return Ok(None);
            }
        }
        let info = self.kv.checkpoint()?;
        if info.written {
            debug!("periodic checkpoint written"; seq = info.seq, keys = info.keys);
        }
        Ok(Some(info))
    }
}
