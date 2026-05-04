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
