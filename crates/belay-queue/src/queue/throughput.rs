//! Trailing-window activity counters for [`QueueInfo::throughput`].
//!
//! The window is split into a fixed number of slots, each covering
//! `window / SLOTS`; a slot is reset when time reaches it again. Counts
//! are therefore exact to one slot width at the trailing edge, which is
//! plenty for a dashboard number, and recording is O(1) with no
//! allocation.
//!
//! [`QueueInfo::throughput`]: belay_proto::QueueInfo::throughput

use std::time::Duration;

use belay_common::Timestamp;
use belay_proto::Throughput;

const SLOTS: usize = 60;

/// What is being counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Metric {
    Enqueued,
    Succeeded,
    /// An attempt failed (including an expired lease), retried or not.
    Failed,
    /// A job died.
    Dead,
}
