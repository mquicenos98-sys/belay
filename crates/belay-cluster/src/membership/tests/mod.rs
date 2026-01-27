//! Unit tests for the state machine, one node at a time. Whole-cluster
//! behaviour is covered by the simulations in `tests/`.
//!
//! Nodes are named by single letters, which map to addresses: "a" is
//! 10.0.0.1, "b" is 10.0.0.2, and so on. The node under test is "a".

mod lifecycle;
mod probe;
mod updates;

use std::collections::{BTreeSet, VecDeque};
use std::sync::Once;
use std::time::Duration;

use belay_common::log::{set_max_level, LevelFilter};

use super::*;
use crate::wire::{decode, MemberRecord};

fn quiet() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| set_max_level(LevelFilter::Warn));
}

const START: i64 = 1_700_000_000_000;

fn at(ms: u64) -> Timestamp {
    Timestamp::from_millis(START) + Duration::from_millis(ms)
}

fn id(s: &str) -> NodeId {
    NodeId::new(s).unwrap()
}

fn addr(name: &str) -> SocketAddr {
    let octet = name.as_bytes()[0] - b'a' + 1;
    SocketAddr::from(([10, 0, 0, octet], 7946))
}

fn config(name: &str) -> Config {
    Config::new(id(name)).with_advertise(addr(name))
}

fn alive(name: &str, incarnation: u64) -> Update {
    Update::Alive {
        id: id(name),
        incarnation,
        addr: format!("{name}:7070"),
        gossip_addr: addr(name),
        meta: Meta::new(),
    }
}

fn suspect(name: &str, incarnation: u64, from: &str) -> Update {
    Update::Suspect {
        id: id(name),
        incarnation,
        from: id(from),
    }
}

fn dead(name: &str, incarnation: u64, from: &str) -> Update {
    Update::Dead {
        id: id(name),
        incarnation,
        from: id(from),
    }
}

fn left(name: &str, incarnation: u64) -> Update {
    Update::Left {
        id: id(name),
        incarnation,
    }
}

fn record(name: &str, state: MemberState, incarnation: u64) -> MemberRecord {
    MemberRecord {
        id: id(name),
        addr: format!("{name}:7070"),
        gossip_addr: addr(name),
        state,
        incarnation,
        meta: Meta::new(),
    }
}

fn ping(seq: u64, target: &str) -> Message {
    Message::Ping {
        seq,
        target: id(target),
    }
}

fn ping_req(seq: u64, target: &str) -> Message {
    Message::PingReq {
        seq,
        target: id(target),
        target_addr: addr(target),
    }
}

fn sync(reply: bool, members: Vec<MemberRecord>) -> Message {
    Message::Sync { reply, members }
}

fn packet(sender: &str, message: Message, updates: Vec<Update>) -> Packet {
    Packet {
        sender: id(sender),
        message,
        updates,
    }
}

fn updates_in(sent: &[(SocketAddr, Packet)]) -> Vec<Update> {
    sent.iter().flat_map(|(_, p)| p.updates.clone()).collect()
}

fn is_sync(p: &Packet, want_reply: bool) -> bool {
    matches!(p.message, Message::Sync { reply, .. } if reply == want_reply)
}

fn is_probe(p: &Packet) -> bool {
    matches!(p.message, Message::Ping { .. } | Message::PingReq { .. })
}

/// Drives one `Membership` through virtual time, recording what it sends.
/// Members listed in `answering` behave like healthy peers: they ack
/// pings, and relay acks for ping requests about other answering members.
struct Driver {
    m: Membership,
    now: Timestamp,
    tag: u32,
    answering: BTreeSet<String>,
    sent: Vec<(SocketAddr, Packet)>,
    last_tick: Option<Timestamp>,
}

impl Driver {
    fn new(config: Config) -> Driver {
        quiet();
        let tag = config.cluster_tag();
        let m = Membership::new(config, Rng::seed_from_u64(42), at(0)).unwrap();
        Driver {
            m,
            now: at(0),
            tag,
            answering: BTreeSet::new(),
            sent: Vec::new(),
            last_tick: None,
        }
    }

    fn named(name: &str) -> Driver {
        Driver::new(config(name))
    }

    /// Introduces peers through gossip; they answer probes.
    fn with_peers(mut self, names: &[&str]) -> Driver {
        for name in names {
            self.gossip(name, alive(name, 0));
            self.answering.insert((*name).to_owned());
        }
        self.sent.clear();
        self.m.drain_events();
        self
    }

    fn name_of(addr: SocketAddr) -> String {
        match addr {
            SocketAddr::V4(v4) => char::from(b'a' + v4.ip().octets()[3] - 1).to_string(),
            SocketAddr::V6(_) => unreachable!("tests use IPv4 addresses"),
        }
    }

    /// Records outbound datagrams (checking their size and encoding) and
    /// plays the answering peers' part.
    fn deliver(&mut self, out: Vec<Outbound>) {
        let mut queue: VecDeque<Outbound> = out.into();
        while let Some(o) = queue.pop_front() {
            assert!(
                o.bytes.len() <= self.m.config().max_packet_size,
                "datagram of {} bytes",
                o.bytes.len()
            );
            let sent = decode(&o.bytes, self.tag).expect("outbound datagrams decode");
            let to = Self::name_of(o.to);
            let answers = self.answering.contains(&to);
            let ack = match &sent.message {
                Message::Ping { seq, target } if answers => {
                    assert_eq!(*target, to.as_str());
                    Some(*seq)
                }
                Message::PingReq {
                    seq, target_addr, ..
                } if answers && self.answering.contains(&Self::name_of(*target_addr)) => Some(*seq),
                _ => None,
            };
            self.sent.push((o.to, sent));
            if let Some(seq) = ack {
                let reply = packet(&to, Message::Ack { seq }, Vec::new());
                queue.extend(self.m.handle(addr(&to), reply, self.now));
            }
        }
    }

    fn tick_until(&mut self, until: Timestamp) {
        loop {
            let next = self.m.next_wakeup();
            if next > until {
                self.now = self.now.max(until);
                return;
            }
            let t = next.max(self.now);
            assert_ne!(self.last_tick, Some(t), "next_wakeup stuck at {t}");
            self.now = t;
            self.last_tick = Some(t);
            let out = self.m.tick(t);
            self.deliver(out);
        }
    }

    fn advance(&mut self, d: Duration) {
        let until = self.now + d;
        self.tick_until(until);
    }

    fn recv(&mut self, from: &str, message: Message, updates: Vec<Update>) {
        let out = self
            .m
            .handle(addr(from), packet(from, message, updates), self.now);
        self.deliver(out);
    }

    fn gossip(&mut self, from: &str, update: Update) {
        self.recv(from, Message::Gossip, vec![update]);
    }

    fn take_sent(&mut self) -> Vec<(SocketAddr, Packet)> {
        std::mem::take(&mut self.sent)
    }

    fn state_of(&self, name: &str) -> Option<MemberState> {
        self.m.member(name).map(|m| m.state)
    }

    /// Advances until a ping to `target` is sent; returns its seq.
    fn run_until_ping_to(&mut self, target: &str, limit: Duration) -> u64 {
        let deadline = self.now + limit;
        loop {
            for (_, p) in self.take_sent() {
                if let Message::Ping { seq, target: t } = p.message {
                    if t == target {
                        return seq;
                    }
                }
            }
            assert!(self.now < deadline, "no ping to {target} before {deadline}");
            let next = self.m.next_wakeup().min(deadline);
            self.tick_until(next);
        }
    }

    /// Stops `name` answering and advances until it is suspected; returns
    /// when that happened.
    fn suspect_silent_member(&mut self, name: &str) -> Timestamp {
        self.answering.remove(name);
        let limit = self.now + Duration::from_secs(30);
        while self.state_of(name) != Some(MemberState::Suspect) {
            assert!(self.now < limit, "{name} was never suspected");
            let next = self.m.next_wakeup();
            self.tick_until(next);
        }
        self.m.member(name).unwrap().since
    }
}
