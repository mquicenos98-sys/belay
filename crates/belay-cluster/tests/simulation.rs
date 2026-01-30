//! Whole-cluster behaviour in deterministic virtual time: joining,
//! crash detection, packet loss, partitions, graceful leave, restarts and
//! ownership. Every node runs with the production defaults (1 s probe
//! interval), so the timings asserted here are the real ones.

mod common;

use std::collections::BTreeSet;
use std::time::Duration;

use belay_cluster::{Faults, LocalState, MemberState, MembershipEvent, Ownership};
use belay_common::log::CaptureLogger;
use common::{keys, Sim};

fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

/// Starts `n` nodes and runs until their views agree.
fn converged_cluster(seed: u64, n: usize) -> Sim {
    let mut sim = Sim::new(seed, n);
    let took = sim.run_until(secs(60), Sim::converged);
    assert!(took.is_some(), "{n} nodes did not converge within 60s");
    sim
}
