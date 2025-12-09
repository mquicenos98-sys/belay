//! Failure detection: probes, indirect probes, relays and suspicion timers.

use super::*;

#[test]
fn answers_pings_for_itself_only() {
    let mut d = Driver::named("a");
    d.recv("b", ping(9, "a"), vec![]);
    let sent = d.take_sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, addr("b"));
    assert_eq!(sent[0].1.message, Message::Ack { seq: 9 });

    // A prober that knows this address under another id gets no answer.
    d.recv("b", ping(10, "zed"), vec![]);
    assert!(d.take_sent().is_empty());
}

#[test]
fn an_acked_probe_raises_no_suspicion() {
    let mut d = Driver::named("a").with_peers(&["b", "c"]);
    d.advance(Duration::from_secs(20));
    let pings = d
        .sent
        .iter()
        .filter(|(_, p)| matches!(p.message, Message::Ping { .. }))
        .count();
    assert!(pings >= 19, "one probe per period, got {pings}");
    assert_eq!(d.m.stats().probe_failures, 0);
    assert_eq!(d.m.stats().suspicions, 0);
    assert_eq!(d.state_of("b"), Some(MemberState::Alive));
    assert_eq!(d.state_of("c"), Some(MemberState::Alive));
}

#[test]
fn probes_visit_every_member_once_per_round() {
    let mut d = Driver::named("a").with_peers(&["b", "c", "d", "e", "f"]);
    d.advance(Duration::from_secs(20));
    let targets: Vec<String> = d
        .sent
        .iter()
        .filter_map(|(_, p)| match &p.message {
            Message::Ping { target, .. } => Some(target.to_string()),
            _ => None,
        })
        .collect();
    assert!(targets.len() >= 15);
    // Rounds after the first (which the peers joined partway through) are
    // permutations of all peers.
    for round in targets[5..15].chunks(5) {
        let distinct: BTreeSet<&String> = round.iter().collect();
        assert_eq!(distinct.len(), 5, "round {round:?}");
    }
}

#[test]
fn a_silent_member_is_probed_indirectly_then_suspected() {
    let mut d = Driver::named("a").with_peers(&["b", "c", "d", "e"]);
    d.answering.remove("e");
    let seq = d.run_until_ping_to("e", Duration::from_secs(10));
    let ping_at = d.now;

    d.tick_until(ping_at + Duration::from_millis(499));
    assert!(!d
        .take_sent()
        .iter()
        .any(|(_, p)| matches!(p.message, Message::PingReq { .. })));
    d.tick_until(ping_at + Duration::from_millis(500));
    let helpers: BTreeSet<SocketAddr> = d
        .take_sent()
        .into_iter()
        .filter(|(_, p)| matches!(p.message, Message::PingReq { .. }))
        .map(|(to, p)| {
            assert_eq!(p.message, ping_req(seq, "e"));
            to
        })
        .collect();
    assert_eq!(helpers.len(), 3);
    assert!(!helpers.contains(&addr("e")));
    assert_eq!(d.m.stats().indirect_probes, 1);

    d.tick_until(ping_at + Duration::from_millis(1_000));
    assert_eq!(d.state_of("e"), Some(MemberState::Suspect));
    assert_eq!(d.m.stats().probe_failures, 1);
    let events = d.m.drain_events();
    assert!(matches!(events.as_slice(), [MembershipEvent::Suspected(m)] if m.id == "e"));
    // The suspect hears about it directly.
    assert!(d
        .take_sent()
        .iter()
        .any(|(to, p)| *to == addr("e") && p.updates.contains(&suspect("e", 0, "a"))));
    // Suspects keep their keys.
    assert!(d.m.ring().contains("e"));
}

#[test]
fn an_indirect_ack_prevents_suspicion() {
    let mut d = Driver::named("a").with_peers(&["b", "c", "d", "e"]);
    // e cannot be reached directly, but a helper relays e's ack.
    d.answering.remove("e");
    let seq = d.run_until_ping_to("e", Duration::from_secs(10));
    let ping_at = d.now;
    d.tick_until(ping_at + Duration::from_millis(500));
    d.recv("b", Message::Ack { seq }, vec![]);
    d.tick_until(ping_at + Duration::from_millis(1_000));
    assert_eq!(d.state_of("e"), Some(MemberState::Alive));
    assert_eq!(d.m.stats().probe_failures, 0);
}

#[test]
fn relays_ping_requests_and_forwards_the_ack() {
    let mut d = Driver::named("a").with_peers(&["b", "c"]);
    d.answering.clear();
    d.recv("b", ping_req(77, "c"), vec![]);
    let sent = d.take_sent();
    let relay_seq = match sent.as_slice() {
        [(to, p)] => match &p.message {
            Message::Ping { seq, target } if *to == addr("c") && *target == "c" => *seq,
            other => panic!("expected a ping to c, got {other:?}"),
        },
        other => panic!("expected one datagram, got {other:?}"),
    };
    d.recv("c", Message::Ack { seq: relay_seq }, vec![]);
    let sent = d.take_sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, addr("b"));
    assert_eq!(sent[0].1.message, Message::Ack { seq: 77 });

    // A second ack for the same relay is not forwarded again.
    d.recv("c", Message::Ack { seq: relay_seq }, vec![]);
    assert!(d.take_sent().is_empty());

    // Asked to probe itself, the node answers the requester directly.
    d.recv("b", ping_req(78, "a"), vec![]);
    assert_eq!(d.take_sent()[0].1.message, Message::Ack { seq: 78 });
}

#[test]
fn relayed_pings_expire_after_a_protocol_period() {
    let mut d = Driver::named("a").with_peers(&["b", "c"]);
    d.answering.clear();
    d.recv("b", ping_req(5, "c"), vec![]);
    let relay_seq = d
        .take_sent()
        .into_iter()
        .find_map(|(_, p)| match p.message {
            Message::Ping { seq, .. } => Some(seq),
            _ => None,
        })
        .unwrap();
    d.advance(Duration::from_millis(1_001));
    d.take_sent();
    d.recv("c", Message::Ack { seq: relay_seq }, vec![]);
    assert!(!d
        .take_sent()
        .iter()
        .any(|(_, p)| p.message == Message::Ack { seq: 5 }));
}

#[test]
fn independent_confirmations_shorten_the_suspicion() {
    let mut d = Driver::named("a").with_peers(&["b", "c", "d", "e"]);
    let since = d.suspect_silent_member("e");
    // The node's own repeated probe failures are not independent.
    d.gossip("b", suspect("e", 0, "a"));
    d.gossip("c", suspect("e", 0, "c"));
    d.gossip("d", suspect("e", 0, "d"));
    let min = d.m.config().suspicion_timeout(5);
    d.tick_until(since + min - Duration::from_millis(1));
    assert_eq!(d.state_of("e"), Some(MemberState::Suspect));
    d.tick_until(since + min);
    assert_eq!(d.state_of("e"), Some(MemberState::Dead));
}

#[test]
fn confirmations_are_gossiped_on() {
    let mut d = Driver::named("a").with_peers(&["b", "c", "d", "e"]);
    d.suspect_silent_member("e");
    d.take_sent();
    d.gossip("c", suspect("e", 0, "c"));
    d.advance(d.m.config().gossip_interval);
    assert!(updates_in(&d.take_sent()).contains(&suspect("e", 0, "c")));
}

#[test]
fn next_wakeup_is_always_in_the_future_after_a_tick() {
    let mut d = Driver::new(config("a").with_seeds([addr("b")])).with_peers(&["b", "c", "d"]);
    d.answering.remove("d");
    let mut now = d.now;
    for _ in 0..500 {
        let out = d.m.tick(now);
        d.deliver(out);
        let next = d.m.next_wakeup();
        assert!(next > now, "next wakeup {next} is not after {now}");
        now = next;
        d.now = now;
    }
    assert!(now > at(10_000));
}

#[test]
fn identical_seeds_and_inputs_give_identical_output() {
    let run = || {
        let mut d = Driver::named("a").with_peers(&["b", "c", "d", "e"]);
        d.answering.remove("c");
        d.advance(Duration::from_secs(40));
        d.sent
            .iter()
            .map(|(to, p)| format!("{to} {p:?}"))
            .collect::<Vec<_>>()
    };
    assert_eq!(run(), run());
}
