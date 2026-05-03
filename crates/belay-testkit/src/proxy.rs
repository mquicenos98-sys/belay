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
