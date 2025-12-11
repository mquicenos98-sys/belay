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

#[test]
fn a_sync_request_is_merged_and_answered_with_the_full_state() {
    let mut d = Driver::named("a").with_peers(&["b"]);
    let members = vec![
        record("c", MemberState::Alive, 2),
        // Remote death reports only raise suspicion here.
        record("b", MemberState::Dead, 0),
        // Unknown suspects are learned, unknown dead are not.
        record("d", MemberState::Suspect, 4),
        record("e", MemberState::Dead, 1),
        record("f", MemberState::Left, 0),
    ];
    d.recv("c", sync(true, members), vec![]);
    assert_eq!(d.state_of("c"), Some(MemberState::Alive));
    assert_eq!(d.state_of("b"), Some(MemberState::Suspect));
    assert_eq!(d.state_of("d"), Some(MemberState::Suspect));
    assert_eq!(d.state_of("e"), None);
    assert_eq!(d.state_of("f"), None);

    let replies: Vec<MemberRecord> = d
        .take_sent()
        .into_iter()
        .filter(|(to, p)| *to == addr("c") && is_sync(p, false))
        .flat_map(|(_, p)| match p.message {
            Message::Sync { members, .. } => members,
            _ => Vec::new(),
        })
        .collect();
    let names: Vec<&str> = replies.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(names, ["a", "b", "c", "d"]);
    assert_eq!(d.m.stats().syncs_received, 1);
}

#[test]
fn tells_a_member_it_believes_dead_so_that_it_refutes() {
    let mut d = Driver::named("a").with_peers(&["b", "c"]);
    d.gossip("c", dead("b", 0, "c"));
    d.take_sent();
    d.recv("b", ping(3, "a"), vec![]);
    let sent = d.take_sent();
    assert!(sent
        .iter()
        .any(|(to, p)| *to == addr("b") && p.updates.contains(&dead("b", 0, "a"))));
    assert!(sent
        .iter()
        .any(|(_, p)| p.message == Message::Ack { seq: 3 }));
    // Once per protocol period.
    d.recv("b", ping(3, "a"), vec![]);
    assert_eq!(d.take_sent().len(), 1);
    // A member that refutes in the same packet is not told.
    d.advance(Duration::from_secs(2));
    d.take_sent();
    d.recv("b", ping(3, "a"), vec![alive("b", 1)]);
    assert_eq!(d.state_of("b"), Some(MemberState::Alive));
    assert_eq!(d.take_sent().len(), 1);
}

#[test]
fn gossip_rounds_reach_fanout_members_including_the_recently_dead() {
    let mut d = Driver::named("a").with_peers(&["b", "c", "d", "e", "f", "g"]);
    d.gossip("b", dead("g", 0, "b"));
    d.take_sent();
    let interval = d.m.config().gossip_interval;
    // Keeps the queue busy with a fresh update per round, and counts how
    // often each address was picked as a gossip target.
    let mut incarnation = 0;
    let mut gossip_rounds = |d: &mut Driver, rounds: usize| {
        let mut targets: BTreeMap<SocketAddr, usize> = BTreeMap::new();
        for _ in 0..rounds {
            incarnation += 1;
            d.gossip("b", alive("b", incarnation));
            d.take_sent();
            d.advance(interval);
            let round: Vec<SocketAddr> = d
                .take_sent()
                .into_iter()
                .filter(|(_, p)| p.message == Message::Gossip)
                .map(|(to, _)| to)
                .collect();
            assert!(round.len() <= 3, "fanout exceeded: {round:?}");
            for to in round {
                *targets.entry(to).or_insert(0) += 1;
            }
        }
        targets
    };
    let early = gossip_rounds(&mut d, 30);
    assert!(
        early.contains_key(&addr("g")),
        "dead member not gossiped to: {early:?}"
    );
    assert!(!early.contains_key(&addr("a")));
    d.advance(d.m.config().gossip_to_dead);
    let late = gossip_rounds(&mut d, 30);
    assert!(
        !late.contains_key(&addr("g")),
        "still gossiping to the dead: {late:?}"
    );
    assert!(late.len() >= 4, "{late:?}");
}

#[test]
fn every_datagram_fits_the_packet_limit() {
    let mut config = config("a");
    config.max_packet_size = 1_200;
    let mut d = Driver::new(config);
    let big_meta: Meta = (0..10)
        .map(|i| (format!("key{i}"), "v".repeat(35)))
        .collect();
    for i in 0..40u8 {
        let name = format!("b{i:02}");
        let gossip_addr = SocketAddr::from(([10, 1, 0, i + 1], 7946));
        let update = Update::Alive {
            id: id(&name),
            incarnation: 0,
            addr: format!("{name}.example.internal:7070"),
            gossip_addr,
            meta: big_meta.clone(),
        };
        let out = d.m.handle(
            gossip_addr,
            packet(&name, Message::Gossip, vec![update]),
            d.now,
        );
        assert!(out.is_empty());
    }
    assert_eq!(d.m.live_count(), 41);
    assert!(d.m.pending_updates() >= 40);
    // A full state reply and some gossip rounds; `deliver` asserts the
    // size and decodability of every datagram.
    let out =
        d.m.handle(addr("c"), packet("c", sync(true, vec![]), vec![]), d.now);
    assert!(out.len() > 1, "41 large records need several datagrams");
    d.deliver(out);
    d.advance(Duration::from_secs(5));
    assert!(d.sent.len() > 20);
}

#[test]
fn logs_joins_suspicions_and_failures() {
    quiet();
    let capture = CaptureLogger::new();
    capture.scope(|| {
        let mut d = Driver::named("a").with_peers(&["b", "c", "d"]);
        let since = d.suspect_silent_member("d");
        d.tick_until(since + Duration::from_secs(60));
    });
    let joined = capture
        .find(|r| r.message == "member joined" && r.field("member") == Some("b"))
        .expect("join logged");
    assert_eq!(joined.level, Level::Info);
    let suspected = capture
        .find(|r| r.message == "member suspected")
        .expect("suspicion logged");
    assert_eq!(suspected.level, Level::Debug);
    assert_eq!(suspected.field("member"), Some("d"));
    let failed = capture
        .find(|r| r.message == "member failed")
        .expect("failure logged");
    assert_eq!(failed.level, Level::Info);
    assert_eq!(failed.field("declared_by"), Some("a"));
}
