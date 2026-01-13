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

impl Suspicion {
    pub(crate) fn new(
        incarnation: u64,
        from: NodeId,
        started: Timestamp,
        min: Duration,
        max: Duration,
        k: u32,
    ) -> Suspicion {
        Suspicion {
            incarnation,
            started,
            min,
            max: max.max(min),
            k,
            suspecters: BTreeSet::from([from]),
            confirmations: 0,
        }
    }

    /// Records that `from` suspects the same incarnation. Returns whether
    /// this was a new confirmation that shortened the timeout, which is
    /// when it is worth gossiping on so that others can count it too.
    pub(crate) fn confirm(&mut self, from: &NodeId) -> bool {
        if self.confirmations >= self.k {
            return false;
        }
        self.suspecters.insert(from.clone());
        self.confirmations += 1;
        true
    }

    pub(crate) fn incarnation(&self) -> u64 {
        self.incarnation
    }

    pub(crate) fn confirmations(&self) -> u32 {
        self.confirmations
    }

    /// The current timeout, measured from when the suspicion started.
    pub(crate) fn timeout(&self) -> Duration {
        if self.k == 0 || self.confirmations >= self.k {
            return self.min;
        }
        let fraction = f64::from(self.confirmations + 1).ln() / f64::from(self.k + 1).ln();
        let span = (self.max - self.min).as_secs_f64();
        let timeout = self.max.as_secs_f64() - span * fraction;
        Duration::from_secs_f64(timeout.max(self.min.as_secs_f64()))
    }

    /// When the suspect is to be declared dead unless it refutes.
    pub(crate) fn deadline(&self) -> Timestamp {
        self.started.saturating_add(self.timeout())
    }
}
