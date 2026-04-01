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
        if self.confirmations >= self.k || self.suspecters.contains(from) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> NodeId {
        NodeId::new(s).unwrap()
    }

    fn suspicion(k: u32) -> Suspicion {
        Suspicion::new(
            3,
            id("a"),
            Timestamp::from_secs(100),
            Duration::from_secs(4),
            Duration::from_secs(24),
            k,
        )
    }

    fn secs(d: Duration) -> f64 {
        (d.as_secs_f64() * 1000.0).round() / 1000.0
    }

    #[test]
    fn starts_at_the_maximum_timeout() {
        let s = suspicion(2);
        assert_eq!(s.timeout(), Duration::from_secs(24));
        assert_eq!(s.deadline(), Timestamp::from_secs(124));
        assert_eq!(s.incarnation(), 3);
        assert_eq!(s.confirmations(), 0);
    }

    #[test]
    fn confirmations_shrink_the_timeout_logarithmically_to_the_minimum() {
        let mut s = suspicion(2);
        assert!(s.confirm(&id("b")));
        // 24 - 20 * ln 2 / ln 3
        assert_eq!(secs(s.timeout()), 11.381);
        assert!(s.confirm(&id("c")));
        assert_eq!(s.timeout(), Duration::from_secs(4));
        assert_eq!(s.deadline(), Timestamp::from_secs(104));
        // Beyond k, confirmations change nothing and are not re-gossiped.
        assert!(!s.confirm(&id("d")));
        assert_eq!(s.confirmations(), 2);
    }

    #[test]
    fn repeated_or_original_suspecters_do_not_count() {
        let mut s = suspicion(3);
        assert!(!s.confirm(&id("a")));
        assert!(s.confirm(&id("b")));
        assert!(!s.confirm(&id("b")));
        assert_eq!(s.confirmations(), 1);
    }

    #[test]
    fn zero_k_or_inverted_bounds_use_the_minimum() {
        let s = suspicion(0);
        assert_eq!(s.timeout(), Duration::from_secs(4));
        let s = Suspicion::new(
            0,
            id("a"),
            Timestamp::EPOCH,
            Duration::from_secs(5),
            Duration::from_secs(1),
            2,
        );
        assert_eq!(s.timeout(), Duration::from_secs(5));
    }
}
