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

impl fmt::Debug for Datagram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Datagram")
            .field("from", &self.from)
            .field("len", &self.bytes.len())
            .finish()
    }
}

/// A datagram socket, shared between the runner's threads.
///
/// Delivery is best effort: datagrams may be lost, duplicated or
/// reordered, and `send_to` succeeding says nothing about arrival. The
/// protocol tolerates all of that.
pub trait Transport: Send + Sync + fmt::Debug {
    /// The address datagrams to this transport are sent to.
    fn local_addr(&self) -> SocketAddr;

    /// Sends one datagram. Errors are reported for local failures only
    /// (e.g. an oversized datagram, an unreachable network); the runner
    /// logs and otherwise ignores them.
    fn send_to(&self, to: SocketAddr, bytes: &[u8]) -> io::Result<()>;

    /// Waits up to `timeout` for a datagram. `Ok(None)` means none arrived
    /// in time.
    fn recv_timeout(&self, timeout: Duration) -> io::Result<Option<Datagram>>;
}

impl<T: Transport + ?Sized> Transport for std::sync::Arc<T> {
    fn local_addr(&self) -> SocketAddr {
        (**self).local_addr()
    }

    fn send_to(&self, to: SocketAddr, bytes: &[u8]) -> io::Result<()> {
        (**self).send_to(to, bytes)
    }

    fn recv_timeout(&self, timeout: Duration) -> io::Result<Option<Datagram>> {
        (**self).recv_timeout(timeout)
    }
}
