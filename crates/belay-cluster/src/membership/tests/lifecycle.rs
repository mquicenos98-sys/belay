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

#[test]
fn leaving_spreads_the_news_then_goes_quiet() {
    let mut d = Driver::named("a").with_peers(&["b", "c", "d"]);
    let out = d.m.leave(d.now);
    assert_eq!(d.m.state(), LocalState::Leaving);
    d.deliver(out);
    assert!(updates_in(&d.take_sent()).contains(&left("a", 0)));
    assert!(!d.m.ring().contains("a"));
    assert_eq!(d.m.leader().unwrap().id, "b");
    // Suspicion no longer triggers refutation while leaving.
    d.gossip("b", suspect("a", 0, "b"));
    assert_eq!(d.m.incarnation(), 0);

    d.advance(Duration::from_secs(5));
    assert_eq!(d.m.state(), LocalState::Left);
    assert!(!d.sent.iter().any(|(_, p)| is_probe(p)));
    // Gone: no output at all.
    assert!(d.m.tick(d.now + Duration::from_secs(60)).is_empty());
    let late = packet("b", ping(1, "a"), vec![]);
    assert!(d.m.handle(addr("b"), late, d.now).is_empty());
    assert_eq!(d.m.next_wakeup(), Timestamp::MAX);
    assert!(d.m.leave(d.now).is_empty());
    assert!(d.m.join(&[addr("b")], d.now).is_empty());
}

#[test]
fn a_lone_node_leaves_immediately() {
    let mut d = Driver::named("a");
    assert!(d.m.leave(d.now).is_empty());
    assert_eq!(d.m.state(), LocalState::Left);
}

#[test]
fn update_meta_bumps_the_incarnation_and_gossips() {
    let mut d = Driver::named("a").with_peers(&["b"]);
    let meta = Meta::from([("weight".to_owned(), "2".to_owned())]);
    d.m.update_meta(meta.clone()).unwrap();
    assert_eq!(d.m.incarnation(), 1);
    assert_eq!(d.m.local_member().meta, meta);
    assert!(matches!(d.m.drain_events().as_slice(), [MembershipEvent::Updated(m)] if m.id == "a"));
    assert_eq!(d.m.ring().weight("a"), Some(2.0));
    d.recv("b", ping(1, "a"), vec![]);
    assert!(updates_in(&d.take_sent())
        .iter()
        .any(|u| matches!(u, Update::Alive { id, meta: m, .. } if *id == "a" && *m == meta)));

    // Unchanged metadata costs nothing.
    let version = d.m.version();
    d.m.update_meta(meta).unwrap();
    assert_eq!((d.m.incarnation(), d.m.version()), (1, version));

    let bad = Meta::from([("weight".to_owned(), "zero".to_owned())]);
    assert!(matches!(d.m.update_meta(bad), Err(Error::InvalidMeta(_))));
    let _ = d.m.leave(d.now);
    assert!(matches!(d.m.update_meta(Meta::new()), Err(Error::Stopped)));
}

#[test]
fn tombstones_are_forgotten_after_the_ttl() {
    let mut config = config("a");
    config.tombstone_ttl = Duration::from_secs(60);
    let mut d = Driver::new(config).with_peers(&["b", "c"]);
    d.gossip("b", dead("c", 0, "b"));
    d.m.drain_events();
    d.advance(Duration::from_secs(59));
    assert_eq!(d.state_of("c"), Some(MemberState::Dead));
    d.advance(Duration::from_secs(2));
    assert_eq!(d.state_of("c"), None);
    assert!(matches!(d.m.drain_events().as_slice(), [MembershipEvent::Reaped(m)] if m.id == "c"));
}

#[test]
fn malformed_and_foreign_datagrams_are_counted_and_dropped() {
    let mut d = Driver::named("a");
    assert!(d.m.handle_datagram(addr("b"), b"garbage", d.now).is_empty());
    let foreign = packet("b", ping(1, "a"), vec![])
        .encode(crate::wire::cluster_tag("other"))
        .unwrap();
    assert!(d.m.handle_datagram(addr("b"), &foreign, d.now).is_empty());
    assert_eq!(d.m.stats().packets_rejected, 2);
    assert_eq!(d.m.stats().packets_received, 0);
    let ours = packet("b", ping(1, "a"), vec![]).encode(d.tag).unwrap();
    assert_eq!(d.m.handle_datagram(addr("b"), &ours, d.now).len(), 1);
    assert_eq!(d.m.stats().packets_received, 1);
}

#[test]
fn packets_carrying_our_own_id_are_ignored() {
    let mut d = Driver::named("a").with_peers(&["b"]);
    let capture = CaptureLogger::new();
    capture.scope(|| {
        // Our own packet, looped back through a seed list naming us.
        d.recv("a", ping(1, "a"), vec![]);
        // Another node configured with our id.
        let impostor = packet("a", Message::Gossip, vec![alive("b", 9)]);
        assert!(d.m.handle(addr("c"), impostor, d.now).is_empty());
    });
    assert!(d.take_sent().is_empty());
    assert_eq!(d.m.member("b").unwrap().incarnation, 0);
    let warnings: Vec<_> = capture
        .records()
        .into_iter()
        .filter(|r| r.message == "another node is using this node's id")
        .collect();
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].field("other"), Some("10.0.0.3:7946"));
}

#[test]
fn a_seed_list_naming_this_node_under_another_address_is_not_a_conflict() {
    // Regression: a node whose seeds included its own loopback address saw
    // its join request come back from 127.0.0.1 and warned that another
    // node was using its id.
    let loopback: SocketAddr = "127.0.0.1:7946".parse().unwrap();
    let mut d = Driver::new(config("a").with_seeds([loopback]));
    let join = d.m.tick(d.now);
    assert_eq!(join.len(), 1);
    assert_eq!(join[0].to, loopback);
    let capture = CaptureLogger::new();
    let echoed = capture.scope(|| d.m.handle_datagram(loopback, &join[0].bytes, d.now));
    assert!(echoed.is_empty());
    assert!(!capture.contains("another node is using this node's id"));
    assert_eq!(d.m.state(), LocalState::Joining);
}
