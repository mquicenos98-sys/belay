//! Gossip over a real UDP socket.

use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use belay_common::sync::lock;

use super::{Datagram, Transport};
use crate::wire::MAX_DATAGRAM;

/// Receive buffer size: any IPv4 UDP payload fits.
const RECV_BUFFER: usize = 65_536;

/// `set_read_timeout` rejects zero; this is the shortest wait used instead.
const MIN_TIMEOUT: Duration = Duration::from_millis(1);

/// A [`Transport`] over a `std::net::UdpSocket`.
///
/// ```
/// use belay_cluster::{Transport, UdpTransport};
/// use std::time::Duration;
///
/// let a = UdpTransport::bind("127.0.0.1:0")?;
/// let b = UdpTransport::bind("127.0.0.1:0")?;
/// a.send_to(b.local_addr(), b"hello")?;
/// let datagram = b.recv_timeout(Duration::from_secs(5))?.expect("delivered on loopback");
/// assert_eq!(datagram.bytes, b"hello");
/// assert_eq!(datagram.from, a.local_addr());
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Debug)]
pub struct UdpTransport {
    socket: UdpSocket,
    local: SocketAddr,
    buf: Mutex<Vec<u8>>,
    /// The read timeout currently set on the socket, in microseconds, so
    /// that the common case of an unchanged timeout costs no system call.
    timeout_us: AtomicU64,
}
