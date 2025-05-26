//! The executor: a fixed set of threads taking cleared jobs off the ready
//! queue, running the handler chain under `catch_unwind`, and reporting.
//!
//! A thread stays busy until its handler returns, even when the job was
//! abandoned meanwhile. Rust threads cannot be killed, so this is what
//! keeps `concurrency` an honest bound on handler threads: a handler that
//! never returns permanently costs one slot (visible as `busy_slots`
//! exceeding the jobs in progress), rather than the worker spawning
//! replacements without limit.

use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::time::Instant;

use belay_common::sync::{lock, wait};
use belay_common::warn;

use crate::context::JobContext;
use crate::worker::report::{report_for, settle};
use crate::worker::state::{Shared, Timing, Tracked};

/// Body of one executor thread.
pub(crate) fn run_executor(shared: &Shared) {
    /// Counts the thread out even if something below panics, so shutdown
    /// never waits for a thread that is gone.
    struct Exit<'a>(&'a Shared);

    impl Drop for Exit<'_> {
        fn drop(&mut self) {
            lock(&self.0.state).exited += 1;
            self.0.changed.notify_all();
        }
    }

    let _exit = Exit(shared);
    loop {
        let job = {
            let mut state = lock(&shared.state);
            loop {
                if let Some(job) = state.ready.pop_front() {
                    state.busy += 1;
                    break job;
                }
                if state.closing {
                    return;
                }
                state = wait(&shared.work, state);
            }
        };
        execute(shared, &job);
        lock(&shared.state).busy -= 1;
        shared.changed.notify_all();
    }
}

/// Runs one job from start to report.
fn execute(shared: &Shared, job: &Arc<Tracked>) {
    let timeout = match (job.job.spec.timeout, job.entry.options.timeout) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let started = Instant::now();
    let timing = Timing {
        timeout,
        deadline: timeout.and_then(|t| started.checked_add(t)),
    };
    if !job.begin(timing) {
        // Abandoned (lease lost or shutdown) between leaving the ready
        // queue and starting; whoever abandoned it has dealt with it.
        release_kind(shared, job);
        return;
    }
    // The watchdog sleeps until the nearest deadline; this may be nearer.
    shared.changed.notify_all();

    let mut ctx = JobContext::new(
        Arc::clone(&job.job),
        shared.config.id.clone(),
        Arc::clone(&job.signals),
        timing.deadline,
        Some(shared.draining.clone()),
    );
    // AssertUnwindSafe: after a panic the context is dropped unused, and
    // the handler's own shared state is the handler's business, as with
    // any thread pool.
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        shared.registry.dispatch(&job.entry, &mut ctx)
    }));
    drop(ctx);
    release_kind(shared, job);

    if !job.finish() {
        warn!(
            "handler returned after its job was abandoned; result dropped";
            job = %job.id(),
            kind = job.kind(),
            ran_for = started.elapsed(),
            reason = job.signals.reason().map(|r| r.to_string()),
            panicked = result.is_err(),
        );
        return;
    }
    let report = report_for(shared, job, result, timing);
    settle(shared, job.id(), job.token, job.kind(), &report);
    shared.forget(job.id());
}

/// Gives the job's kind slot back, possibly clearing a parked job.
fn release_kind(shared: &Shared, job: &Tracked) {
    let promoted = lock(&shared.state).release_kind(job.kind());
    if promoted {
        shared.work.notify_all();
    }
}
