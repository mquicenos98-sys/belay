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

impl fmt::Debug for MemNetwork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = lock(&self.net.state);
        f.debug_struct("MemNetwork")
            .field("endpoints", &state.mailboxes.len())
            .field("faults", &state.faults)
            .field("stats", &state.stats)
            .finish()
    }
}

impl MemNetwork {
    /// A fault-free network on the system clock whose random decisions
    /// derive from `seed`.
    #[must_use]
    pub fn new(seed: u64) -> MemNetwork {
        MemNetwork::with_clock(seed, system_clock())
    }

    /// A fault-free network whose delivery times follow `clock`.
    #[must_use]
    pub fn with_clock(seed: u64, clock: SharedClock) -> MemNetwork {
        MemNetwork {
            net: Arc::new(Net {
                state: Mutex::new(NetState {
                    rng: Rng::seed_from_u64(seed),
                    faults: Faults::default(),
                    mailboxes: HashMap::new(),
                    blocked: HashSet::new(),
                    isolated: HashSet::new(),
                    stats: NetStats::default(),
                    seq: 0,
                    next_host: 0,
                }),
                arrived: Condvar::new(),
                clock,
            }),
        }
    }

    /// Attaches an endpoint at `addr`; fails with `AddrInUse` if one is
    /// already there.
    pub fn bind(&self, addr: SocketAddr) -> io::Result<MemTransport> {
        let mut state = lock(&self.net.state);
        if state.mailboxes.contains_key(&addr) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("{addr} is already bound"),
            ));
        }
        state.mailboxes.insert(addr, Mailbox::new());
        Ok(MemTransport {
            network: self.clone(),
            addr,
        })
    }

    /// Attaches an endpoint at the next free address of the form
    /// `10.x.y.z:7946`.
    #[must_use]
    pub fn bind_next(&self) -> MemTransport {
        let mut state = lock(&self.net.state);
        loop {
            state.next_host += 1;
            let [_, x, y, z] = state.next_host.to_be_bytes();
            let addr = SocketAddr::from((Ipv4Addr::new(10, x, y, z), DEFAULT_PORT));
            if let std::collections::hash_map::Entry::Vacant(slot) = state.mailboxes.entry(addr) {
                slot.insert(Mailbox::new());
                return MemTransport {
                    network: self.clone(),
                    addr,
                };
            }
        }
    }

    /// The current fault settings.
    #[must_use]
    pub fn faults(&self) -> Faults {
        lock(&self.net.state).faults.clone()
    }

    /// Replaces the fault settings for datagrams sent from now on.
    pub fn set_faults(&self, faults: Faults) {
        lock(&self.net.state).faults = faults;
    }

    /// Sets the probability that a datagram is lost.
    pub fn set_drop_rate(&self, p: f64) {
        lock(&self.net.state).faults.drop_rate = p;
    }

    /// Sets the probability that a datagram is delivered twice.
    pub fn set_duplicate_rate(&self, p: f64) {
        lock(&self.net.state).faults.duplicate_rate = p;
    }

    /// Sets the base latency and the random jitter added to it.
    pub fn set_latency(&self, latency: Duration, jitter: Duration) {
        let mut state = lock(&self.net.state);
        state.faults.latency = latency;
        state.faults.jitter = jitter;
    }

    /// Cuts every link between a member of `a` and a member of `b`, in
    /// both directions.
    pub fn partition(&self, a: &[SocketAddr], b: &[SocketAddr]) {
        let mut state = lock(&self.net.state);
        for &x in a {
            for &y in b {
                state.blocked.insert((x, y));
                state.blocked.insert((y, x));
            }
        }
    }

    /// Cuts the one-way link from `from` to `to`.
    pub fn block(&self, from: SocketAddr, to: SocketAddr) {
        lock(&self.net.state).blocked.insert((from, to));
    }

    /// Cuts every link to and from `addr`.
    pub fn isolate(&self, addr: SocketAddr) {
        lock(&self.net.state).isolated.insert(addr);
    }

    /// Restores every link cut by [`partition`](Self::partition),
    /// [`block`](Self::block) or [`isolate`](Self::isolate).
    pub fn heal(&self) {
        let mut state = lock(&self.net.state);
        state.blocked.clear();
        state.isolated.clear();
    }

    /// Traffic counters.
    #[must_use]
    pub fn stats(&self) -> NetStats {
        lock(&self.net.state).stats
    }

    /// Datagrams sent but not yet received.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        lock(&self.net.state)
            .mailboxes
            .values()
            .map(BinaryHeap::len)
            .sum()
    }

    /// The earliest delivery time among datagrams in flight, for
    /// simulations that jump the clock from event to event.
    #[must_use]
    pub fn next_delivery(&self) -> Option<Timestamp> {
        let state = lock(&self.net.state);
        state
            .mailboxes
            .values()
            .filter_map(|m| m.peek().map(|r| r.0.deliver_at))
            .min()
    }

    /// The clock deciding delivery times.
    #[must_use]
    pub fn clock(&self) -> &SharedClock {
        &self.net.clock
    }
}

/// One endpoint of a [`MemNetwork`]. Dropping it unbinds the address;
/// datagrams still in flight to it are discarded.
pub struct MemTransport {
    network: MemNetwork,
    addr: SocketAddr,
}

impl fmt::Debug for MemTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemTransport")
            .field("addr", &self.addr)
            .finish()
    }
}

impl MemTransport {
    /// The next datagram due for delivery at the network clock's current
    /// time, without waiting.
    #[must_use]
    pub fn try_recv(&self) -> Option<Datagram> {
        let now = self.network.net.clock.now();
        lock(&self.network.net.state).pop_due(self.addr, now)
    }

    /// The network this endpoint belongs to.
    #[must_use]
    pub fn network(&self) -> &MemNetwork {
        &self.network
    }
}

impl Transport for MemTransport {
    fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    fn send_to(&self, to: SocketAddr, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() > MAX_DATAGRAM {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "datagram of {} bytes exceeds the {MAX_DATAGRAM}-byte limit",
                    bytes.len()
                ),
            ));
        }
        let net = &self.network.net;
        let now = net.clock.now();
        lock(&net.state).send(self.addr, to, bytes, now);
        net.arrived.notify_all();
        Ok(())
    }

    fn recv_timeout(&self, timeout: Duration) -> io::Result<Option<Datagram>> {
        let net = &self.network.net;
        let deadline = Instant::now() + timeout;
        let mut state = lock(&net.state);
        loop {
            let now = net.clock.now();
            if let Some(datagram) = state.pop_due(self.addr, now) {
                return Ok(Some(datagram));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            // Sleep until the next datagram for us is due (by the network
            // clock), a send wakes us, or the timeout.
            let wait = match state.next_for(self.addr) {
                Some(due) => left.min(
                    due.saturating_duration_since(now)
                        .max(Duration::from_millis(1)),
                ),
                None => left,
            };
            state = wait_timeout(&net.arrived, state, wait).0;
        }
    }
}

impl Drop for MemTransport {
    fn drop(&mut self) {
        lock(&self.network.net.state).mailboxes.remove(&self.addr);
        self.network.net.arrived.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_common::ManualClock;
    use std::thread;

    fn manual() -> (ManualClock, MemNetwork) {
        let clock = ManualClock::new(Timestamp::from_secs(1_000));
        let net = MemNetwork::with_clock(1, clock.shared());
        (clock, net)
    }

    fn drain(t: &MemTransport) -> Vec<Vec<u8>> {
        std::iter::from_fn(|| t.try_recv())
            .map(|d| d.bytes)
            .collect()
    }

    #[test]
    fn delivers_in_order_without_faults() {
        let (_, net) = manual();
        let a = net.bind_next();
        let b = net.bind_next();
        assert_ne!(a.local_addr(), b.local_addr());
        for i in 0..5u8 {
            a.send_to(b.local_addr(), &[i]).unwrap();
        }
        let d = b.try_recv().unwrap();
        assert_eq!(d.from, a.local_addr());
        assert_eq!(d.bytes, [0]);
        assert_eq!(drain(&b), [[1], [2], [3], [4]]);
        let stats = net.stats();
        assert_eq!((stats.sent, stats.delivered), (5, 5));
    }

    #[test]
    fn datagrams_to_unbound_addresses_vanish() {
        let (_, net) = manual();
        let a = net.bind_next();
        a.send_to("10.9.9.9:1".parse().unwrap(), b"x").unwrap();
        assert_eq!(net.stats().undeliverable, 1);
        assert_eq!(net.in_flight(), 0);
    }

    #[test]
    fn binding_an_address_twice_fails_until_the_first_is_dropped() {
        let (_, net) = manual();
        let addr: SocketAddr = "10.0.0.1:7946".parse().unwrap();
        let first = net.bind(addr).unwrap();
        assert_eq!(net.bind(addr).unwrap_err().kind(), io::ErrorKind::AddrInUse);
        // bind_next skips explicitly bound addresses.
        assert_ne!(net.bind_next().local_addr(), addr);
        drop(first);
        net.bind(addr).unwrap();
    }

    #[test]
    fn latency_holds_datagrams_until_due() {
        let (clock, net) = manual();
        net.set_latency(Duration::from_millis(20), Duration::ZERO);
        let a = net.bind_next();
        let b = net.bind_next();
        let sent_at = clock.now();
        a.send_to(b.local_addr(), b"x").unwrap();
        assert_eq!(
            net.next_delivery(),
            Some(sent_at + Duration::from_millis(20))
        );
        clock.advance(Duration::from_millis(19));
        assert!(b.try_recv().is_none());
        clock.advance(Duration::from_millis(1));
        assert!(b.try_recv().is_some());
        assert_eq!(net.next_delivery(), None);
    }

    #[test]
    fn loss_rate_is_honoured_and_reproducible() {
        let run = |seed| {
            let clock = ManualClock::new(Timestamp::EPOCH);
            let net = MemNetwork::with_clock(seed, clock.shared());
            net.set_drop_rate(0.1);
            let a = net.bind_next();
            let b = net.bind_next();
            let mut received = Vec::new();
            for i in 0..10_000u32 {
                a.send_to(b.local_addr(), &i.to_le_bytes()).unwrap();
                if let Some(d) = b.try_recv() {
                    received.push(d.bytes);
                }
            }
            (net.stats(), received)
        };
        let (stats, received) = run(5);
        assert!((850..=1_150).contains(&stats.dropped), "{}", stats.dropped);
        assert_eq!(received.len() as u64, 10_000 - stats.dropped);
        assert_eq!(run(5).1, received, "same seed, same losses");
        assert_ne!(run(6).1, received);
    }

    #[test]
    fn duplication_delivers_two_copies() {
        let (_, net) = manual();
        net.set_duplicate_rate(1.0);
        let a = net.bind_next();
        let b = net.bind_next();
        a.send_to(b.local_addr(), b"twice").unwrap();
        assert_eq!(drain(&b), [b"twice".to_vec(), b"twice".to_vec()]);
        assert_eq!(net.stats().duplicated, 1);
    }

    #[test]
    fn reordering_lets_later_datagrams_overtake() {
        let (clock, net) = manual();
        let a = net.bind_next();
        let b = net.bind_next();
        net.set_faults(Faults {
            reorder_rate: 1.0,
            reorder_delay: Duration::from_millis(10),
            ..Faults::default()
        });
        a.send_to(b.local_addr(), b"first").unwrap();
        net.set_faults(Faults::default());
        a.send_to(b.local_addr(), b"second").unwrap();
        clock.advance(Duration::from_millis(10));
        assert_eq!(drain(&b), [b"second".to_vec(), b"first".to_vec()]);
        assert_eq!(net.stats().reordered, 1);
    }

    #[test]
    fn jitter_stays_within_bounds() {
        let (clock, net) = manual();
        net.set_latency(Duration::from_millis(5), Duration::from_millis(10));
        let a = net.bind_next();
        let b = net.bind_next();
        for _ in 0..200 {
            a.send_to(b.local_addr(), b"j").unwrap();
        }
        clock.advance(Duration::from_millis(4));
        assert!(b.try_recv().is_none());
        clock.advance(Duration::from_millis(11));
        assert_eq!(drain(&b).len(), 200);
    }

    #[test]
    fn partitions_block_both_directions_until_healed() {
        let (_, net) = manual();
        let a = net.bind_next();
        let b = net.bind_next();
        let c = net.bind_next();
        net.partition(&[a.local_addr()], &[b.local_addr(), c.local_addr()]);
        a.send_to(b.local_addr(), b"x").unwrap();
        b.send_to(a.local_addr(), b"x").unwrap();
        b.send_to(c.local_addr(), b"same side").unwrap();
        assert!(a.try_recv().is_none());
        assert!(b.try_recv().is_none());
        assert_eq!(drain(&c).len(), 1);
        assert_eq!(net.stats().blocked, 2);

        net.heal();
        a.send_to(b.local_addr(), b"x").unwrap();
        assert!(b.try_recv().is_some());

        net.block(a.local_addr(), c.local_addr());
        a.send_to(c.local_addr(), b"x").unwrap();
        c.send_to(a.local_addr(), b"x").unwrap();
        assert!(c.try_recv().is_none());
        assert!(a.try_recv().is_some(), "blocks are one-way");

        net.isolate(c.local_addr());
        b.send_to(c.local_addr(), b"x").unwrap();
        c.send_to(b.local_addr(), b"x").unwrap();
        assert!(b.try_recv().is_none() && c.try_recv().is_none());
    }

    #[test]
    fn blocking_receive_wakes_on_send_from_another_thread() {
        let net = MemNetwork::new(3);
        let a = net.bind_next();
        let b = net.bind_next();
        let to = b.local_addr();
        let sender = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            a.send_to(to, b"wake").unwrap();
            a
        });
        let d = b.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(d.map(|d| d.bytes), Some(b"wake".to_vec()));
        drop(sender.join().unwrap());
        let start = Instant::now();
        assert!(b.recv_timeout(Duration::from_millis(20)).unwrap().is_none());
        assert!(start.elapsed() >= Duration::from_millis(15));
    }

    #[test]
    fn blocking_receive_waits_out_latency_on_the_system_clock() {
        let net = MemNetwork::new(4);
        net.set_latency(Duration::from_millis(30), Duration::ZERO);
        let a = net.bind_next();
        let b = net.bind_next();
        let start = Instant::now();
        a.send_to(b.local_addr(), b"slow").unwrap();
        let d = b.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(d.is_some());
        assert!(start.elapsed() >= Duration::from_millis(25));
    }

    #[test]
    fn rejects_oversized_datagrams() {
        let (_, net) = manual();
        let a = net.bind_next();
        let err = a
            .send_to(a.local_addr(), &vec![0; MAX_DATAGRAM + 1])
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(net.stats().sent, 0);
    }
}
