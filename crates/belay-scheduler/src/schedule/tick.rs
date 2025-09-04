//! The tick: turning due fire times into jobs.
//!
//! A tick looks at every schedule whose `next_fire_at` has come, sorts its
//! due fire times into stale (vetoed by a calendar that changed), paused,
//! missed and on-time ones, applies the misfire and overlap policies,
//! dispatches, and moves the schedule to its next fire time. All the
//! bookkeeping of one tick is saved in a single batch at the end; events
//! are emitted only once it is saved. If the save fails nothing is
//! recorded, and the next tick repeats the same fires, which the
//! dispatcher deduplicates by their unique keys.

use std::time::Duration;

use belay_common::sync::lock;
use belay_common::{debug, warn, Timestamp};
use belay_proto::event::{EventKind, SkipReason as EventSkipReason};
use belay_proto::{MisfirePolicy, OverlapPolicy, Schedule, ScheduleId};
use belay_store::WriteBatch;

use super::plan::{Next, Plan, Span, COUNT_LIMIT};
use super::{Entry, Fired, Scheduler, SkipReason, Skipped, TickError, TickErrorKind, TickReport};
use crate::store;

/// The due fire times of one schedule, sorted by what happens to them.
struct Due {
    /// Times vetoed by a calendar that changed after they were computed.
    stale: Option<Span>,
    /// Times that came due while the schedule was paused.
    paused: Option<Span>,
    /// Times missed by more than the misfire threshold.
    missed: Option<Span>,
    /// The oldest missed times: as many as the policy may fire, plus one.
    missed_head: Vec<Timestamp>,
    /// Times due within the threshold, oldest first.
    on_time: Vec<Timestamp>,
    /// The first fire time after the due ones.
    next: Next,
}
