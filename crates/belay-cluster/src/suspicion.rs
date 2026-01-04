//! Suspicion timers whose timeout shrinks with independent confirmations.
//!
//! This is the "dynamic suspicion timeout" from Lifeguard (Dadgar et al.,
//! 2018): a suspicion starts with the maximum timeout, and every other
//! member that independently fails to reach the suspect shortens it, down
//! to the minimum once `k` confirmations have arrived:
//!
//! ```text
//! timeout(c) = max(min, max - (max - min) * ln(c + 1) / ln(k + 1))
//! ```
//!
//! A single member with a bad network link therefore needs a long time to
//! get a healthy member declared dead (giving the suspect ample time to
//! refute), while a member that really crashed, which every prober notices,
//! is declared dead after roughly the minimum timeout.

use std::collections::BTreeSet;
use std::time::Duration;

use belay_common::Timestamp;

use crate::member::NodeId;

#[derive(Clone, Debug)]
pub(crate) struct Suspicion {
    incarnation: u64,
    started: Timestamp,
    min: Duration,
    max: Duration,
    k: u32,
    /// Every node that suspected this incarnation, the first included.
    suspecters: BTreeSet<NodeId>,
    /// Suspecters after the first, capped at `k`.
    confirmations: u32,
}
