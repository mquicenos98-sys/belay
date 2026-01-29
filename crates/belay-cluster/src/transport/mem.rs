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
