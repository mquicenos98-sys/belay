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
