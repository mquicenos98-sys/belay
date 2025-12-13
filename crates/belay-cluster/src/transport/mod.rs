//! Datagram transports for the [`Node`](crate::Node) runner.
//!
//! The membership protocol needs only best-effort, unordered, size-limited
//! datagrams, i.e. UDP semantics: [`UdpTransport`] is the real thing, and
//! [`MemNetwork`] is an in-process network with seeded loss, duplication,
//! latency, reordering and partitions for tests.

mod mem;

mod udp;

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

pub use mem::{Faults, MemNetwork, MemTransport, NetStats};
pub use udp::UdpTransport;

/// One received datagram.
#[derive(Clone, PartialEq, Eq)]
pub struct Datagram {
    /// The sender's address.
    pub from: SocketAddr,
    /// The payload.
    pub bytes: Vec<u8>,
}
