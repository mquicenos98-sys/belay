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
