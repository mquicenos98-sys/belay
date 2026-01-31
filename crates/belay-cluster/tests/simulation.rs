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

#[test]
fn clusters_of_5_10_and_20_converge_after_joining_through_one_seed() {
    for n in [5, 10, 20] {
        let mut sim = Sim::new(n as u64, n);
        let took = sim
            .run_until(secs(60), Sim::converged)
            .unwrap_or_else(|| panic!("{n} nodes did not converge"));
        // Joining is one round trip to the seed plus gossip about the
        // newcomers; it should take a few protocol periods, not minutes.
        assert!(took < secs(15), "{n} nodes took {took:?}");
        for i in sim.running() {
            assert_eq!(sim.nodes[i].membership.state(), LocalState::Active);
            assert_eq!(sim.live_set(i).len(), n);
        }
        // Converged views give identical ownership and leadership.
        let keys = keys(500);
        let reference = sim.owners(0, &keys);
        for i in 1..n {
            assert_eq!(
                sim.owners(i, &keys),
                reference,
                "node {i} disagrees on ownership"
            );
            assert_eq!(
                sim.nodes[i].membership.leader().map(|m| m.id.to_string()),
                Some(common::name(0))
            );
        }
        // Stays converged with nothing happening.
        sim.run_for(secs(60));
        assert!(sim.converged());
        assert_eq!(sim.failures(), 0);
    }
}

#[test]
fn a_crashed_node_is_declared_dead_by_everyone_within_the_bound() {
    let n = 10;
    let mut sim = converged_cluster(11, n);
    let victim = 3;
    let config = sim.nodes[0].membership.config().clone();
    let bound = config.failure_detection_bound(n);
    let keys = keys(2_000);
    let before = sim.ring(0);

    let killed_at = sim.now();
    sim.kill(victim);
    let took = sim
        .run_until(bound, |s| s.all_see(victim, MemberState::Dead))
        .unwrap_or_else(|| panic!("not every node declared the victim dead within {bound:?}"));
    // Nobody may declare it dead before the minimum suspicion timeout.
    assert!(
        took >= config.suspicion_timeout(n),
        "declared dead after only {took:?}"
    );

    for i in sim.running() {
        let failed_at = sim.nodes[i]
            .events
            .iter()
            .find(|(_, e)| matches!(e, MembershipEvent::Failed(m) if m.id == sim.nodes[victim].id))
            .map(|(at, _)| *at)
            .unwrap_or_else(|| panic!("node {i} emitted no failure event"));
        assert!(failed_at.saturating_duration_since(killed_at) <= bound);
    }
    assert!(sim
        .events_about(victim)
        .iter()
        .all(|(_, e)| !matches!(e, MembershipEvent::Left(_))));

    // Only the dead node's keys moved, and every node agrees where.
    let after = sim.ring(0);
    let diff = Ownership::diff(&before, &after, &keys);
    let victim_id = sim.nodes[victim].id.as_str();
    assert!(diff.moved() > 0);
    assert_eq!(diff.lost_by(victim_id).count(), diff.moved());
    let reference = sim.owners(0, &keys);
    for i in sim.running() {
        assert_eq!(sim.owners(i, &keys), reference);
    }
}

#[test]
fn ten_percent_packet_loss_causes_no_false_deaths() {
    let mut sim = converged_cluster(21, 10);
    sim.net.set_drop_rate(0.10);
    sim.run_for(secs(300));
    assert_eq!(sim.failures(), 0, "a live node was declared dead");
    let (suspicions, refutations) = sim.total_stats();
    assert!(suspicions > 0, "the loss should cause some suspicion");
    assert!(refutations > 0, "suspects should have refuted");
    sim.net.set_drop_rate(0.0);
    assert!(sim.run_until(secs(30), Sim::converged).is_some());
    let dropped = sim.net.stats().dropped;
    assert!(dropped > 1_000, "only {dropped} datagrams dropped");
}

#[test]
fn duplication_jitter_and_reordering_do_not_disturb_a_healthy_cluster() {
    let mut sim = converged_cluster(31, 8);
    sim.net.set_faults(Faults {
        duplicate_rate: 0.2,
        reorder_rate: 0.2,
        reorder_delay: Duration::from_millis(150),
        latency: Duration::from_millis(2),
        jitter: Duration::from_millis(30),
        ..Faults::default()
    });
    sim.run_for(secs(180));
    assert_eq!(sim.failures(), 0);
    assert!(sim.converged());
    let stats = sim.net.stats();
    assert!(stats.duplicated > 0 && stats.reordered > 0);
}

#[test]
fn a_healed_partition_reunites_the_cluster() {
    let n = 10;
    let mut sim = converged_cluster(41, n);
    let left: Vec<usize> = (0..5).collect();
    let right: Vec<usize> = (5..n).collect();
    let addrs = |ids: &[usize]| ids.iter().map(|&i| sim.nodes[i].addr).collect::<Vec<_>>();
    let (a, b) = (addrs(&left), addrs(&right));
    sim.net.partition(&a, &b);

    // Each side declares the other side dead...
    let split = sim.run_until(secs(180), |s| {
        left.iter().all(|&i| {
            right
                .iter()
                .all(|&j| s.state(i, j) == Some(MemberState::Dead))
        }) && right.iter().all(|&j| {
            left.iter()
                .all(|&i| s.state(j, i) == Some(MemberState::Dead))
        })
    });
    assert!(split.is_some(), "the sides never gave up on each other");
    // ...but keeps working on its own, with its own owners.
    for &i in &left {
        assert_eq!(sim.live_set(i).len(), 5);
    }
    let incarnations: Vec<u64> = (0..n).map(|j| sim.incarnation(j, j).unwrap()).collect();

    sim.net.heal();
    let healed = sim.run_until(secs(180), Sim::converged);
    assert!(
        healed.is_some(),
        "the cluster did not reunite after the partition healed"
    );
    // Members came back by refuting their deaths with higher incarnations.
    let refuted = (0..n)
        .filter(|&j| sim.incarnation(0, j).unwrap() > incarnations[j])
        .count();
    assert!(refuted >= 5, "only {refuted} members refuted");
    let keys = keys(500);
    let reference = sim.owners(0, &keys);
    for i in 1..n {
        assert_eq!(sim.owners(i, &keys), reference);
    }
    // And they stay together.
    sim.run_for(secs(60));
    assert!(sim.converged());
}

#[test]
fn a_briefly_unreachable_node_refutes_suspicion_and_stays_alive() {
    let mut sim = converged_cluster(51, 10);
    let target = 7;
    let addr = sim.nodes[target].addr;
    sim.net.isolate(addr);
    let suspected = sim.run_until(secs(30), |s| {
        s.running()
            .any(|i| i != target && s.state(i, target) == Some(MemberState::Suspect))
    });
    assert!(
        suspected.is_some(),
        "nobody noticed the node was unreachable"
    );
    sim.net.heal();
    sim.run_for(secs(60));
    assert!(sim
        .events_about(target)
        .iter()
        .all(|(_, e)| !matches!(e, MembershipEvent::Failed(_))));
    assert!(sim.all_see(target, MemberState::Alive));
    assert!(sim.nodes[target].membership.incarnation() >= 1);
    assert!(sim
        .events_about(target)
        .iter()
        .any(|(_, e)| matches!(e, MembershipEvent::Recovered(_))));
}

#[test]
fn a_graceful_leave_is_reported_as_left_not_dead() {
    let mut sim = converged_cluster(61, 8);
    let leaver = 5;
    sim.leave(leaver);
    let took = sim
        .run_until(secs(10), |s| s.all_see(leaver, MemberState::Left))
        .expect("the departure did not reach everyone");
    assert!(took < secs(3), "leaving took {took:?}");
    // The leaver keeps gossiping until its retransmissions are used up,
    // which takes a few more gossip rounds than reaching everyone.
    let stopped = sim.run_until(secs(5), |s| !s.nodes[leaver].is_running());
    assert!(stopped.is_some(), "the leaver never finished leaving");
    assert_eq!(sim.nodes[leaver].membership.state(), LocalState::Left);
    sim.run_for(secs(120));
    assert!(sim.all_see(leaver, MemberState::Left));
    assert!(sim.converged());
    let about = sim.events_about(leaver);
    assert!(about.iter().all(|(_, e)| !matches!(
        e,
        MembershipEvent::Failed(_) | MembershipEvent::Suspected(_)
    )));
    assert_eq!(
        about
            .iter()
            .filter(|(_, e)| matches!(e, MembershipEvent::Left(_)))
            .count(),
        7
    );
}
