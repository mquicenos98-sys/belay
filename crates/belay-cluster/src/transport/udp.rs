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

impl UdpTransport {
    /// Binds a socket to `addr` (e.g. `"0.0.0.0:7946"`; port 0 picks a
    /// free port).
    ///
    /// A node bound to an unspecified address (`0.0.0.0`, `::`) must be
    /// given an [`advertise`](crate::Config::advertise) address that peers
    /// can reach.
    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<UdpTransport> {
        UdpTransport::from_socket(UdpSocket::bind(addr)?)
    }

    /// Wraps an already bound socket, e.g. one configured with custom
    /// buffer sizes.
    pub fn from_socket(socket: UdpSocket) -> io::Result<UdpTransport> {
        let local = socket.local_addr()?;
        socket.set_nonblocking(false)?;
        Ok(UdpTransport {
            socket,
            local,
            buf: Mutex::new(vec![0; RECV_BUFFER]),
            timeout_us: AtomicU64::new(0),
        })
    }

    /// The underlying socket.
    #[must_use]
    pub fn socket(&self) -> &UdpSocket {
        &self.socket
    }

    fn set_timeout(&self, timeout: Duration) -> io::Result<()> {
        let us = u64::try_from(timeout.as_micros()).unwrap_or(u64::MAX);
        if self.timeout_us.swap(us, Ordering::Relaxed) != us {
            if let Err(e) = self.socket.set_read_timeout(Some(timeout)) {
                self.timeout_us.store(0, Ordering::Relaxed);
                return Err(e);
            }
        }
        Ok(())
    }
}

impl Transport for UdpTransport {
    fn local_addr(&self) -> SocketAddr {
        self.local
    }

    fn send_to(&self, to: SocketAddr, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() > MAX_DATAGRAM {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "datagram of {} bytes exceeds the {MAX_DATAGRAM}-byte UDP limit",
                    bytes.len()
                ),
            ));
        }
        self.socket.send_to(bytes, to).map(|_| ())
    }

    fn recv_timeout(&self, timeout: Duration) -> io::Result<Option<Datagram>> {
        let timeout = timeout.max(MIN_TIMEOUT);
        let deadline = Instant::now() + timeout;
        let mut buf = lock(&self.buf);
        self.set_timeout(timeout)?;
        loop {
            match self.socket.recv_from(&mut buf) {
                Ok((n, from)) => {
                    return Ok(Some(Datagram {
                        from,
                        bytes: buf[..n].to_vec(),
                    }))
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None)
                }
                // Windows reports an ICMP "port unreachable" caused by an
                // earlier send (to a node that has gone) as a reset on the
                // next receive; it says nothing about this receive.
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::ConnectionReset | io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e),
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            self.set_timeout(left.max(MIN_TIMEOUT))?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exchanges_datagrams_on_loopback() {
        let a = UdpTransport::bind("127.0.0.1:0").unwrap();
        let b = UdpTransport::bind("127.0.0.1:0").unwrap();
        assert_ne!(a.local_addr().port(), 0);
        for i in 0..5u8 {
            a.send_to(b.local_addr(), &[i; 100]).unwrap();
        }
        let mut got = Vec::new();
        while got.len() < 5 {
            let d = b
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .expect("loopback delivers");
            assert_eq!(d.from, a.local_addr());
            got.push(d.bytes[0]);
        }
        got.sort_unstable();
        assert_eq!(got, [0, 1, 2, 3, 4]);
    }

    #[test]
    fn recv_times_out_quietly() {
        let a = UdpTransport::bind("127.0.0.1:0").unwrap();
        let start = Instant::now();
        assert!(a.recv_timeout(Duration::from_millis(20)).unwrap().is_none());
        assert!(start.elapsed() >= Duration::from_millis(15));
        // A zero timeout is raised to the minimum rather than rejected.
        assert!(a.recv_timeout(Duration::ZERO).unwrap().is_none());
    }

    #[test]
    fn sending_to_a_closed_port_does_not_break_receiving() {
        // Regression: on Windows the ICMP error for the first send surfaced
        // as ConnectionReset from the next recv and stopped the receiver.
        let a = UdpTransport::bind("127.0.0.1:0").unwrap();
        let gone = UdpTransport::bind("127.0.0.1:0").unwrap();
        let gone_addr = gone.local_addr();
        drop(gone);
        a.send_to(gone_addr, b"anyone?").unwrap();
        let b = UdpTransport::bind("127.0.0.1:0").unwrap();
        b.send_to(a.local_addr(), b"still here").unwrap();
        let d = a
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .expect("datagram after the reset");
        assert_eq!(d.bytes, b"still here");
    }

    #[test]
    fn rejects_oversized_datagrams() {
        let a = UdpTransport::bind("127.0.0.1:0").unwrap();
        let err = a
            .send_to(a.local_addr(), &vec![0; MAX_DATAGRAM + 1])
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
