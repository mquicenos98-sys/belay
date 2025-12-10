//! Applying updates and full-state records: precedence, refutation,
//! gossip and packing.

use std::collections::BTreeMap;

use belay_common::log::{CaptureLogger, Level};

use super::*;

#[test]
fn an_alive_record_with_our_incarnation_but_other_data_is_refuted() {
    // Regression: a node restarted with the same id and incarnation but a
    // new address was shadowed by its old record forever.
    let mut d = Driver::named("a").with_peers(&["b"]);
    let mut stale = alive("a", 0);
    if let Update::Alive { addr: a, .. } = &mut stale {
        *a = "old-host:7070".into();
    }
    d.gossip("b", stale);
    assert_eq!(d.m.incarnation(), 1);
    // "a:7070" differs from the configured (empty) API address too.
    d.gossip("b", alive("a", 1));
    assert_eq!(d.m.incarnation(), 2);
}

#[test]
fn a_higher_incarnation_clears_suspicion() {
    let mut d = Driver::named("a").with_peers(&["b", "c", "d"]);
    d.suspect_silent_member("d");
    d.m.drain_events();
    // A stale alive does not help...
    d.gossip("c", alive("d", 0));
    assert_eq!(d.state_of("d"), Some(MemberState::Suspect));
    // ...a refutation does.
    d.gossip("c", alive("d", 1));
    assert_eq!(d.state_of("d"), Some(MemberState::Alive));
    assert!(matches!(
        d.m.drain_events().as_slice(),
        [MembershipEvent::Recovered(m)] if m.incarnation == 1
    ));
    // And the old suspicion timer is gone.
    d.answering.insert("d".into());
    d.advance(Duration::from_secs(30));
    assert_eq!(d.state_of("d"), Some(MemberState::Alive));
}

#[test]
fn dead_members_come_back_only_with_a_higher_incarnation() {
    let mut d = Driver::named("a").with_peers(&["b", "c"]);
    d.gossip("b", dead("c", 0, "b"));
    assert_eq!(d.state_of("c"), Some(MemberState::Dead));
    d.gossip("c", alive("c", 0));
    assert_eq!(d.state_of("c"), Some(MemberState::Dead));
    // Suspicion of a dead member changes nothing.
    d.gossip("b", suspect("c", 5, "b"));
    assert_eq!(d.state_of("c"), Some(MemberState::Dead));
    d.m.drain_events();
    d.gossip("c", alive("c", 1));
    assert_eq!(d.state_of("c"), Some(MemberState::Alive));
    assert!(matches!(d.m.drain_events().as_slice(), [MembershipEvent::Joined(m)] if m.id == "c"));
    assert!(d.m.ring().contains("c"));
}

#[test]
fn updates_about_unknown_members_other_than_alive_are_ignored() {
    let mut d = Driver::named("a");
    d.gossip("b", suspect("x", 1, "b"));
    d.gossip("b", dead("y", 1, "b"));
    d.gossip("b", left("z", 1));
    assert_eq!(d.m.members().count(), 1);
    assert!(d.m.drain_events().is_empty());
}

#[test]
fn metadata_changes_need_a_new_incarnation() {
    let mut d = Driver::named("a").with_peers(&["b"]);
    let with_zone = |incarnation| {
        let mut update = alive("b", incarnation);
        if let Update::Alive { meta, .. } = &mut update {
            meta.insert("zone".into(), "x".into());
        }
        update
    };
    d.gossip("b", with_zone(0));
    assert!(d.m.member("b").unwrap().meta.is_empty());
    d.gossip("b", with_zone(1));
    assert_eq!(d.m.member("b").unwrap().meta["zone"], "x");
    assert!(matches!(d.m.drain_events().as_slice(), [MembershipEvent::Updated(m)] if m.id == "b"));
}

#[test]
fn departed_members_are_not_probed_and_own_nothing() {
    let mut d = Driver::named("a").with_peers(&["b", "c"]);
    d.gossip("b", left("b", 0));
    assert_eq!(d.state_of("b"), Some(MemberState::Left));
    assert!(matches!(d.m.drain_events().as_slice(), [MembershipEvent::Left(m)] if m.id == "b"));
    assert!(!d.m.ring().contains("b"));
    d.take_sent();
    d.advance(Duration::from_secs(10));
    assert!(!d
        .take_sent()
        .iter()
        .any(|(to, p)| *to == addr("b") && is_probe(p)));
    // A later death report does not turn a departure into a failure.
    d.gossip("c", dead("b", 0, "c"));
    assert_eq!(d.state_of("b"), Some(MemberState::Left));
}
