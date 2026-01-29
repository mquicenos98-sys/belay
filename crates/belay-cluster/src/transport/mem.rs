//! An in-process datagram network with injectable faults.
//!
//! Every fault decision (loss, duplication, delay) is drawn from one seeded
//! [`Rng`] at send time, and delivery times come from the network's
//! [`Clock`]. With a [`ManualClock`](belay_common::ManualClock) and a
//! single thread driving all nodes, a run is fully reproducible: the
//! simulation tests step every node in virtual time and replay failures
//! from their seed. With the system clock the same network serves the
//! threaded [`Node`](crate::Node) tests.
//!
//! Partitions are checked when a datagram is sent; datagrams already in
//! flight when a partition starts are still delivered, as on a real
//! network whose link failed behind them.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use belay_common::sync::{lock, wait_timeout};
use belay_common::time::system_clock;
use belay_common::{Clock, Rng, SharedClock, Timestamp};

use super::{Datagram, Transport};
use crate::wire::MAX_DATAGRAM;

/// Port used by addresses from [`MemNetwork::bind_next`].
const DEFAULT_PORT: u16 = 7946;

/// Fault settings applied to every datagram sent.
#[derive(Clone, Debug, PartialEq)]
pub struct Faults {
    /// Probability that a datagram is lost.
    pub drop_rate: f64,
    /// Probability that a datagram is delivered twice (each copy with its
    /// own delay).
    pub duplicate_rate: f64,
    /// Probability that a datagram is held back an extra
    /// [`reorder_delay`](Self::reorder_delay), letting later ones overtake
    /// it.
    pub reorder_rate: f64,
    /// Extra delay of reordered datagrams.
    pub reorder_delay: Duration,
    /// Base one-way latency.
    pub latency: Duration,
    /// Uniformly random extra latency in `[0, jitter]`, which also
    /// reorders datagrams sent close together.
    pub jitter: Duration,
}

impl Default for Faults {
    fn default() -> Self {
        Faults {
            drop_rate: 0.0,
            duplicate_rate: 0.0,
            reorder_rate: 0.0,
            reorder_delay: Duration::from_millis(10),
            latency: Duration::ZERO,
            jitter: Duration::ZERO,
        }
    }
}

/// Counters of what the network did with the datagrams it was given.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetStats {
    /// Datagrams handed to `send_to`.
    pub sent: u64,
    /// Datagrams handed to a receiver.
    pub delivered: u64,
    /// Datagrams lost to `drop_rate`.
    pub dropped: u64,
    /// Extra copies created by `duplicate_rate`.
    pub duplicated: u64,
    /// Datagrams held back by `reorder_rate`.
    pub reordered: u64,
    /// Datagrams stopped by a partition, block or isolation.
    pub blocked: u64,
    /// Datagrams to an address nobody is bound to.
    pub undeliverable: u64,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct InFlight {
    deliver_at: Timestamp,
    /// Send order, which breaks ties so equal delivery times stay FIFO.
    seq: u64,
    from: SocketAddr,
    bytes: Vec<u8>,
}

type Mailbox = BinaryHeap<Reverse<InFlight>>;

struct NetState {
    rng: Rng,
    faults: Faults,
    mailboxes: HashMap<SocketAddr, Mailbox>,
    blocked: HashSet<(SocketAddr, SocketAddr)>,
    isolated: HashSet<SocketAddr>,
    stats: NetStats,
    seq: u64,
    next_host: u32,
}

impl NetState {
    fn delay(&mut self) -> Duration {
        let mut delay = self.faults.latency;
        let jitter = u64::try_from(self.faults.jitter.as_millis()).unwrap_or(u64::MAX);
        if jitter > 0 {
            delay += Duration::from_millis(self.rng.gen_range(0..=jitter));
        }
        if self.rng.gen_bool(self.faults.reorder_rate) {
            self.stats.reordered += 1;
            delay += self.faults.reorder_delay;
        }
        delay
    }

    fn send(&mut self, from: SocketAddr, to: SocketAddr, bytes: &[u8], now: Timestamp) {
        self.stats.sent += 1;
        if self.isolated.contains(&from)
            || self.isolated.contains(&to)
            || self.blocked.contains(&(from, to))
        {
            self.stats.blocked += 1;
            return;
        }
        if !self.mailboxes.contains_key(&to) {
            self.stats.undeliverable += 1;
            return;
        }
        if self.rng.gen_bool(self.faults.drop_rate) {
            self.stats.dropped += 1;
            return;
        }
        let copies = if self.rng.gen_bool(self.faults.duplicate_rate) {
            self.stats.duplicated += 1;
            2
        } else {
            1
        };
        for _ in 0..copies {
            let deliver_at = now + self.delay();
            self.seq += 1;
            let item = InFlight {
                deliver_at,
                seq: self.seq,
                from,
                bytes: bytes.to_vec(),
            };
            if let Some(mailbox) = self.mailboxes.get_mut(&to) {
                mailbox.push(Reverse(item));
            }
        }
    }

    fn pop_due(&mut self, at: SocketAddr, now: Timestamp) -> Option<Datagram> {
        let mailbox = self.mailboxes.get_mut(&at)?;
        if mailbox.peek()?.0.deliver_at > now {
            return None;
        }
        let Reverse(item) = mailbox.pop()?;
        self.stats.delivered += 1;
        Some(Datagram {
            from: item.from,
            bytes: item.bytes,
        })
    }

    fn next_for(&self, at: SocketAddr) -> Option<Timestamp> {
        self.mailboxes.get(&at)?.peek().map(|r| r.0.deliver_at)
    }
}

struct Net {
    state: Mutex<NetState>,
    arrived: Condvar,
    clock: SharedClock,
}

/// A simulated network connecting [`MemTransport`]s. Clones share it.
///
/// ```
/// use belay_cluster::{MemNetwork, Transport};
/// use belay_common::{ManualClock, Timestamp};
/// use std::time::Duration;
///
/// let clock = ManualClock::new(Timestamp::from_secs(0));
/// let net = MemNetwork::with_clock(7, clock.shared());
/// net.set_latency(Duration::from_millis(5), Duration::ZERO);
/// let a = net.bind_next();
/// let b = net.bind_next();
///
/// a.send_to(b.local_addr(), b"ping").unwrap();
/// assert!(b.try_recv().is_none(), "still in flight");
/// clock.advance(Duration::from_millis(5));
/// assert_eq!(b.try_recv().unwrap().bytes, b"ping");
///
/// net.partition(&[a.local_addr()], &[b.local_addr()]);
/// a.send_to(b.local_addr(), b"lost").unwrap();
/// clock.advance(Duration::from_secs(1));
/// assert!(b.try_recv().is_none());
/// ```
#[derive(Clone)]
pub struct MemNetwork {
    net: Arc<Net>,
}
