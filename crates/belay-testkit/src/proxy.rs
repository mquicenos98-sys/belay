//! [`TcpProxy`]: dropped connections on demand, relayed with two threads
//! per connection.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use belay_common::sync::lock;

use crate::error::Result;

/// Longest request head kept per connection.
const MAX_HEAD: usize = 8 * 1024;

#[derive(Default)]
struct State {
    /// Both sockets of every relayed connection, for cutting.
    live: Mutex<Vec<(u64, TcpStream, TcpStream)>>,
    heads: Mutex<Vec<String>>,
    accepted: AtomicU64,
    refusing: AtomicBool,
    closing: AtomicBool,
}

/// A TCP relay between a client and a harness server that a test can cut,
/// to see how clients cope with dropped connections.
///
/// Each accepted connection gets its own connection to the target and two
/// relay threads. [`cut`](TcpProxy::cut) shuts every live connection down
/// in both directions, like a network blip; new connections are relayed
/// again straight away unless [`refuse`](TcpProxy::refuse) is on. The
/// proxy also keeps the head of the first request on each connection, so
/// a test can check what a reconnecting client sent (`Last-Event-ID`).
///
/// ```
/// use belay_testkit::{TcpProxy, TestServer};
///
/// let server = TestServer::start()?;
/// let proxy = TcpProxy::start(server.addr())?;
/// let client = belay_testkit::connect(&proxy.url(), None)?;
/// client.health()?;
/// assert_eq!(proxy.cut(), 1); // the client's pooled connection
/// client.health()?; // a fresh connection through the proxy
/// assert!(proxy.request_heads()[0].starts_with("GET /healthz HTTP/1.1"));
/// # Ok::<(), belay_testkit::Error>(())
/// ```
pub struct TcpProxy {
    addr: SocketAddr,
    target: SocketAddr,
    state: Arc<State>,
    acceptor: Option<thread::JoinHandle<()>>,
}

impl TcpProxy {
    /// Listens on `127.0.0.1:0` and relays to `target`.
    pub fn start(target: SocketAddr) -> Result<TcpProxy> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let addr = listener.local_addr()?;
        let state = Arc::new(State::default());
        let accepting = Arc::clone(&state);
        let acceptor = thread::Builder::new()
            .name("belay-testkit-proxy".to_owned())
            .spawn(move || accept_loop(&listener, target, &accepting))?;
        Ok(TcpProxy {
            addr,
            target,
            state,
            acceptor: Some(acceptor),
        })
    }

    /// The proxy's address.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// `http://` plus the proxy's address, for a client's base URL.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Where connections are relayed to.
    #[must_use]
    pub fn target(&self) -> SocketAddr {
        self.target
    }

    /// Shuts down every live connection; returns how many there were.
    pub fn cut(&self) -> usize {
        let live = std::mem::take(&mut *lock(&self.state.live));
        for (_, client, server) in &live {
            let _ = client.shutdown(Shutdown::Both);
            let _ = server.shutdown(Shutdown::Both);
        }
        live.len()
    }

    /// While on, new connections are closed as soon as they are accepted.
    pub fn refuse(&self, refusing: bool) {
        self.state.refusing.store(refusing, Ordering::SeqCst);
    }

    /// Connections accepted so far, refused ones included.
    #[must_use]
    pub fn connections(&self) -> u64 {
        self.state.accepted.load(Ordering::SeqCst)
    }

    /// Connections being relayed right now.
    #[must_use]
    pub fn live_connections(&self) -> usize {
        lock(&self.state.live).len()
    }

    /// The head (request line and headers) of the first request on each
    /// relayed connection, in the order connections were made.
    #[must_use]
    pub fn request_heads(&self) -> Vec<String> {
        lock(&self.state.heads).clone()
    }
}

impl Drop for TcpProxy {
    fn drop(&mut self) {
        self.state.closing.store(true, Ordering::SeqCst);
        // Wake the acceptor, which then sees the flag and returns.
        let _ = TcpStream::connect(self.addr);
        if let Some(acceptor) = self.acceptor.take() {
            let _ = acceptor.join();
        }
        self.cut();
    }
}

impl fmt::Debug for TcpProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TcpProxy")
            .field("addr", &self.addr)
            .field("target", &self.target)
            .field("live", &self.live_connections())
            .finish()
    }
}

fn accept_loop(listener: &TcpListener, target: SocketAddr, state: &Arc<State>) {
    let mut next_id = 0u64;
    for incoming in listener.incoming() {
        if state.closing.load(Ordering::SeqCst) {
            return;
        }
        let Ok(client) = incoming else { continue };
        state.accepted.fetch_add(1, Ordering::SeqCst);
        if state.refusing.load(Ordering::SeqCst) {
            let _ = client.shutdown(Shutdown::Both);
            continue;
        }
        let Ok(server) = TcpStream::connect(target) else {
            let _ = client.shutdown(Shutdown::Both);
            continue;
        };
        next_id += 1;
        if let Err(e) = relay(next_id, client, server, state) {
            belay_common::debug!("proxy relay failed to start"; error = %e);
        }
    }
}

fn relay(id: u64, client: TcpStream, server: TcpStream, state: &Arc<State>) -> io::Result<()> {
    let _ = client.set_nodelay(true);
    let _ = server.set_nodelay(true);
    lock(&state.live).push((id, client.try_clone()?, server.try_clone()?));
    let upstream = (client.try_clone()?, server.try_clone()?);
    let heads = Arc::clone(state);
    thread::Builder::new()
        .name("belay-testkit-proxy-up".to_owned())
        .spawn(move || {
            let (from, to) = upstream;
            pump(from, to, Some(&heads.heads));
        })?;
    let finished = Arc::clone(state);
    thread::Builder::new()
        .name("belay-testkit-proxy-down".to_owned())
        .spawn(move || {
            pump(server, client, None);
            lock(&finished.live).retain(|(live, _, _)| *live != id);
        })?;
    Ok(())
}

/// Copies `from` to `to` until either side closes, then closes both
/// directions so the peer notices. With `heads`, the first request head
/// is recorded.
fn pump(mut from: TcpStream, mut to: TcpStream, heads: Option<&Mutex<Vec<String>>>) {
    let mut head: Option<Vec<u8>> = heads.map(|_| Vec::new());
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if let (Some(bytes), Some(heads)) = (head.as_mut(), heads) {
            bytes.extend_from_slice(&buf[..n]);
            if let Some(end) = find_head_end(bytes) {
                lock(heads).push(String::from_utf8_lossy(&bytes[..end]).into_owned());
                head = None;
            } else if bytes.len() > MAX_HEAD {
                head = None;
            }
        }
        if to.write_all(&buf[..n]).is_err() {
            break;
        }
    }
    let _ = to.shutdown(Shutdown::Both);
    let _ = from.shutdown(Shutdown::Both);
}

fn find_head_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|w| w == b"\r\n\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A server that answers every connection with its first line echoed.
    fn echo_server() -> SocketAddr {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                thread::spawn(move || {
                    let mut stream = stream;
                    let mut buf = [0u8; 1024];
                    while let Ok(n) = stream.read(&mut buf) {
                        if n == 0 || stream.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                });
            }
        });
        addr
    }

    fn round_trip(stream: &mut TcpStream, text: &[u8]) -> io::Result<Vec<u8>> {
        stream.write_all(text)?;
        let mut back = vec![0u8; text.len()];
        stream.read_exact(&mut back)?;
        Ok(back)
    }

    #[test]
    fn relays_both_ways_and_records_request_heads() {
        let proxy = TcpProxy::start(echo_server()).unwrap();
        let mut stream = TcpStream::connect(proxy.addr()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let request = b"GET /x HTTP/1.1\r\nLast-Event-ID: 42\r\n\r\n";
        assert_eq!(round_trip(&mut stream, request).unwrap(), request);
        assert_eq!(
            proxy.request_heads(),
            ["GET /x HTTP/1.1\r\nLast-Event-ID: 42"]
        );
        assert_eq!(proxy.connections(), 1);
    }

    #[test]
    fn cutting_ends_live_connections_and_refusing_blocks_new_ones() {
        let proxy = TcpProxy::start(echo_server()).unwrap();
        let mut stream = TcpStream::connect(proxy.addr()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        round_trip(&mut stream, b"hello").unwrap();
        assert_eq!(proxy.cut(), 1);
        assert!(round_trip(&mut stream, b"again").is_err());

        proxy.refuse(true);
        let mut refused = TcpStream::connect(proxy.addr()).unwrap();
        refused
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        assert!(round_trip(&mut refused, b"x").is_err());
        proxy.refuse(false);
        let mut fresh = TcpStream::connect(proxy.addr()).unwrap();
        fresh
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        assert_eq!(round_trip(&mut fresh, b"back").unwrap(), b"back");
    }
}
