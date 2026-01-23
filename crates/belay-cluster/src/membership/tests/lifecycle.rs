//! The local node's lifecycle: creation, joining, metadata, leaving,
//! tombstones and hostile input.

use belay_common::log::CaptureLogger;

use super::*;

#[test]
fn a_node_without_seeds_is_active_and_alone() {
    let d = Driver::named("a");
    assert_eq!(d.m.state(), LocalState::Active);
    assert!(d.m.is_joined());
    assert_eq!(d.m.live_count(), 1);
    assert_eq!(d.m.leader().unwrap().id, "a");
    assert_eq!(d.m.ring().owner("any"), Some(&id("a")));
    assert_eq!(d.m.incarnation(), 0);
}

#[test]
fn creation_requires_an_advertise_address_and_a_valid_config() {
    let err = Membership::new(Config::new(id("a")), Rng::seed_from_u64(1), at(0)).unwrap_err();
    assert!(
        err.to_string().contains("advertise address is not set"),
        "{err}"
    );
    let mut bad = config("a");
    bad.probe_timeout = bad.probe_interval * 2;
    assert!(Membership::new(bad, Rng::seed_from_u64(1), at(0)).is_err());
}

#[test]
fn contacts_seeds_until_one_answers() {
    let mut d = Driver::new(config("a").with_seeds([addr("b"), addr("a")]));
    assert_eq!(d.m.state(), LocalState::Joining);
    assert_eq!(d.m.seeds(), &[addr("b")], "own address is not a seed");
    d.tick_until(at(0));
    let syncs: Vec<_> = d
        .take_sent()
        .into_iter()
        .filter(|(_, p)| is_sync(p, true))
        .collect();
    assert_eq!(syncs.len(), 1);
    assert_eq!(syncs[0].0, addr("b"));
    assert_eq!(
        syncs[0].1.message,
        sync(true, vec![MemberRecord::from(d.m.local_member())])
    );

    // No answer: retried after join_retry_interval.
    d.advance(Duration::from_millis(1_000));
    assert!(d
        .take_sent()
        .iter()
        .any(|(to, p)| *to == addr("b") && is_sync(p, true)));

    d.recv(
        "b",
        sync(false, vec![record("b", MemberState::Alive, 3)]),
        vec![],
    );
    assert_eq!(d.m.state(), LocalState::Active);
    assert_eq!(d.state_of("b"), Some(MemberState::Alive));
    assert!(matches!(d.m.drain_events().as_slice(), [MembershipEvent::Joined(m)] if m.id == "b"));

    // Joined: no more join attempts while b is alive.
    d.answering.insert("b".into());
    d.take_sent();
    d.advance(Duration::from_secs(5));
    assert!(!d.take_sent().iter().any(|(_, p)| is_sync(p, true)));
}

#[test]
fn a_node_left_alone_keeps_knocking_on_its_seeds() {
    let mut d = Driver::new(config("a").with_seeds([addr("b")])).with_peers(&["b"]);
    d.recv("b", sync(false, vec![]), vec![]);
    assert!(d.m.is_joined());
    d.gossip("c", dead("b", 0, "c"));
    assert_eq!(d.m.live_count(), 1);
    d.take_sent();
    d.advance(d.m.config().reconnect_interval + Duration::from_secs(1));
    assert!(d
        .take_sent()
        .iter()
        .any(|(to, p)| *to == addr("b") && is_sync(p, true)));
}

#[test]
fn disabling_reconnects_also_stops_knocking_on_seeds() {
    let mut config = config("a").with_seeds([addr("b")]);
    config.reconnect_interval = Duration::ZERO;
    let mut d = Driver::new(config).with_peers(&["b"]);
    d.recv("b", sync(false, vec![]), vec![]);
    d.gossip("c", dead("b", 0, "c"));
    d.take_sent();
    d.advance(Duration::from_secs(60));
    assert!(!d.take_sent().iter().any(|(_, p)| is_sync(p, true)));
}

#[test]
fn explicit_join_sends_full_state_to_new_seeds() {
    let mut d = Driver::named("a").with_peers(&["b"]);
    let out = d.m.join(&[addr("c"), addr("a")], d.now);
    d.deliver(out);
    let sent = d.take_sent();
    assert_eq!(sent.len(), 1, "own address is skipped");
    assert_eq!(sent[0].0, addr("c"));
    match &sent[0].1.message {
        Message::Sync {
            reply: true,
            members,
        } => assert_eq!(members.len(), 2),
        other => panic!("expected a sync, got {other:?}"),
    }
    assert_eq!(d.m.seeds(), &[addr("c")]);
    assert_eq!(d.m.stats().syncs_sent, 1);
}
