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
