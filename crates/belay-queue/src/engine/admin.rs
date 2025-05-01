//! Operator actions on jobs (cancel, retry, promote, delete) and queues
//! (pause, resume, configure, purge, delete).
//!
//! Job actions are idempotent where that is unambiguous: cancelling a
//! cancelled job, promoting a ready one or pausing a paused queue succeed
//! without changing anything, so a retried HTTP request does no harm.
//! Actions that make no sense for the job's state fail with
//! [`Error::WrongState`], naming the states that would allow them.
//!
//! Bulk removals (`purge`, `delete_queue`) run in batches of
//! [`EngineOptions::batch_size`](crate::EngineOptions::batch_size) jobs,
//! releasing the engine lock between batches so that workers are not
//! stalled behind a large purge. They are therefore not atomic as a
//! whole; each batch is.

use std::collections::BTreeSet;
use std::ops::Bound;

use belay_common::text::plural;
use belay_proto::event::EventKind;
use belay_proto::{Job, JobId, JobState, QueueConfig, QueueInfo, QueueName, Validate};

use super::QueueEngine;
use crate::error::{Error, Result};
use crate::state::State;

/// Most index entries one purge batch looks at, so a purge of a rare
/// state in a huge queue still releases the lock regularly.
const SCAN_BUDGET_PER_BATCH: usize = 16;
