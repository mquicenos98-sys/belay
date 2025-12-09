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
