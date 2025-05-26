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
