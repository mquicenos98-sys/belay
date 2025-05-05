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
