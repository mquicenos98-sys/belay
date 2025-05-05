//! [`QueueEngine::verify`]: an offline-style consistency check run
//! against the live engine.
//!
//! The engine's indexes are derived data, rebuilt from the store on every
//! open. Verification performs that rebuild into a scratch state and
//! compares the result with what the engine holds, which catches any bug
//! that lets memory and store drift apart without restarting the process.
//! It backs `belay admin verify`, and the randomized tests call it after
//! every step.

use std::collections::BTreeSet;

use belay_store::WriteBatch;

use super::open::{load_claims, load_jobs, load_queues};
use super::QueueEngine;
use crate::error::Result;
use crate::records::printable;
use crate::state::State;

/// Most problems listed in a [`VerifyReport`]; the rest are counted.
const MAX_PROBLEMS: usize = 100;

/// Outcome of [`QueueEngine::verify`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[must_use]
pub struct VerifyReport {
    /// Jobs found in the store.
    pub jobs: u64,
    /// Queues found in the store (records, plus queues implied by jobs).
    pub queues: u64,
    /// Valid uniqueness claims found in the store.
    pub claims: u64,
    /// One line per inconsistency, at most 100; empty when all is well.
    pub problems: Vec<String>,
    /// Inconsistencies beyond those listed.
    pub more_problems: u64,
}

impl VerifyReport {
    /// Whether memory and store agree.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        self.problems.is_empty()
    }

    fn problem(&mut self, line: String) {
        if self.problems.len() < MAX_PROBLEMS {
            self.problems.push(line);
        } else {
            self.more_problems += 1;
        }
    }
}

impl QueueEngine {
    /// Rebuilds every index from the store and compares it with the live
    /// one, reporting each difference.
    ///
    /// This holds the engine lock for a full scan of the store, so every
    /// other operation waits for it: it is a diagnostic to run on demand,
    /// not periodically. The comparison covers job states and scheduling
    /// fields, per-queue counts and indexes, configurations and uniqueness
    /// claims; it skips the in-memory-only parts (fairness position, token
    /// buckets, throughput windows, the worker registry).
    pub fn verify(&self) -> Result<VerifyReport> {
        let core = self.lock();
        let live = &core.st;
        let now = self.clock.now();
        let mut fresh = State::new(self.opts.throughput_window, 0);
        let mut repair = WriteBatch::new();
        load_queues(&*self.kv, &self.layout, &mut fresh, now)?;
        let holders = load_jobs(
            &*self.kv,
            &self.layout,
            &mut fresh,
            &mut repair,
            now,
            self.opts.open_chunk,
        )?;
        let claims = load_claims(
            &*self.kv,
            &self.layout,
            &mut fresh,
            &holders,
            &mut repair,
            now,
        )?;

        let mut report = VerifyReport {
            jobs: fresh.jobs.len() as u64,
            queues: fresh.queues.len() as u64,
            claims: claims as u64,
            ..VerifyReport::default()
        };
        for op in repair.ops() {
            report.problem(format!(
                "store record {} is stale or missing",
                printable(op.key())
            ));
        }

        let ids: BTreeSet<_> = live.jobs.keys().chain(fresh.jobs.keys()).collect();
        for id in ids {
            match (live.jobs.get(id), fresh.jobs.get(id)) {
                (Some(_), None) => report.problem(format!("job {id} is indexed but not stored")),
                (None, Some(_)) => report.problem(format!("job {id} is stored but not indexed")),
                (Some(mem), Some(disk)) if mem != disk => report.problem(format!(
                    "job {id} is indexed as {} in {} but stored as {} in {}",
                    mem.state, mem.queue, disk.state, disk.queue
                )),
                _ => {}
            }
        }

        let names: BTreeSet<_> = live.queues.keys().chain(fresh.queues.keys()).collect();
        for name in names {
            match (live.queues.get(name), fresh.queues.get(name)) {
                (Some(_), None) => report.problem(format!("queue {name} exists only in memory")),
                (None, Some(_)) => report.problem(format!("queue {name} exists only in the store")),
                (Some(mem), Some(disk)) => {
                    for index in mem.differences(disk) {
                        report.problem(format!("queue {name}: {index} differ from the store"));
                    }
                }
                (None, None) => {}
            }
        }
        Ok(report)
    }
}
