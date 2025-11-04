//! The HTTP/1.1 server: listener, accept loop, connection threads and
//! graceful shutdown. The concurrency model is documented on [`Server`].

mod config;

mod conn;

mod pool;

use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::handler::Handler;
use crate::sync::ShutdownSignal;

pub use config::{ErrorHook, ServerConfig, ServerError};
use conn::{Counters, Shared};
use pool::{RejectReason, WorkerPool};

/// Pause after an `accept` error such as descriptor exhaustion, so the loop
/// does not spin while the condition lasts.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// How a server's shutdown ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drain {
    /// Every connection finished within the grace period.
    Complete,
    /// The grace period ran out with connections still open; their threads
    /// were left to finish on their own.
    TimedOut {
        /// Connections still open.
        remaining: usize,
    },
}

/// A snapshot of server counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ServerStats {
    /// Connections currently open.
    pub active_connections: usize,
    /// Connections accepted since start.
    pub accepted: u64,
    /// Connections refused with 503 because the server was at capacity.
    pub rejected: u64,
    /// Request heads received.
    pub requests: u64,
    /// Requests answered with a 4xx/5xx by the server itself because they
    /// were malformed, too large or too slow.
    pub protocol_errors: u64,
}

/// A bound listener, ready to serve.
///
/// # Concurrency model
///
/// One thread per connection, drawn from an elastic pool capped at
/// [`ServerConfig::max_connections`]. A connection keeps its thread across
/// keep-alive requests and for the whole life of a streaming response, so a
/// handler may block (a 30-second long-poll lease, an hour-long event
/// stream) without affecting other clients. The cap is what bounds memory
/// and threads: beyond it, new connections get a canned `503` with
/// `Retry-After: 1` straight from the accept thread, and above 7/8 of it
/// responses stop offering keep-alive so idle clients hand their slots
/// back. Size `max_connections` for the expected number of concurrent
/// long-poll workers and event subscribers plus ordinary traffic. Threads
/// idle for [`ServerConfig::thread_idle_timeout`] exit, so the pool shrinks
/// again after a burst.
///
/// # Shutdown
///
/// [`ShutdownSignal::trigger`] (or [`ServerHandle::shutdown`]) wakes the
/// blocked `accept` by connecting to the listener from the inside, then the
/// listener is closed. Idle keep-alive connections notice within
/// [`ServerConfig::poll_interval`] and close; requests already being
/// received or handled run to completion and are answered with
/// `Connection: close`; streaming bodies see
/// [`BodyWriter::is_shutting_down`](crate::BodyWriter::is_shutting_down),
/// and blocking handlers can wait on the [`ShutdownSignal`] stored in every
/// request's extensions. After [`ServerConfig::shutdown_grace`] the server
/// stops waiting and reports the connections it abandoned.
///
/// ```
/// use belay_http::{Request, Response, Server, ServerConfig};
///
/// let server = Server::bind("127.0.0.1:0", ServerConfig::default())?;
/// let addr = server.local_addr();
/// let handle = server.spawn(|_: &mut Request| Response::text("ok"))?;
/// assert_eq!(handle.local_addr(), addr);
/// handle.shutdown_and_join();
/// # Ok::<(), belay_http::Error>(())
/// ```
pub struct Server {
    listener: TcpListener,
    local_addr: SocketAddr,
    config: ServerConfig,
    signal: ShutdownSignal,
}

impl Server {
    /// Binds to `addr` (use port 0 for an ephemeral port).
    pub fn bind<A: ToSocketAddrs>(addr: A, config: ServerConfig) -> Result<Server> {
        config.validate()?;
        let addrs: Vec<SocketAddr> = addr
            .to_socket_addrs()
            .map_err(|source| Error::Bind {
                addr: "(unresolvable address)".into(),
                source,
            })?
            .collect();
        let describe = || {
            addrs
                .iter()
                .map(SocketAddr::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        };
        let listener = TcpListener::bind(&addrs[..]).map_err(|source| Error::Bind {
            addr: describe(),
            source,
        })?;
        Server::from_listener(listener, config)
    }

    /// Serves on an already-bound listener.
    pub fn from_listener(listener: TcpListener, config: ServerConfig) -> Result<Server> {
        config.validate()?;
        listener.set_nonblocking(false)?;
        Ok(Server {
            local_addr: listener.local_addr()?,
            listener,
            config,
            signal: ShutdownSignal::new(),
        })
    }

    /// The bound address (with the actual port when bound to port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The signal that stops this server; trigger it from another thread
    /// to end [`run`](Self::run).
    pub fn shutdown_signal(&self) -> ShutdownSignal {
        self.signal.clone()
    }

    /// Serves on the current thread until the shutdown signal triggers,
    /// then drains.
    pub fn run<H: Handler + 'static>(self, handler: H) -> Drain {
        self.start(Arc::new(handler)).serve()
    }

    /// Serves on a background thread.
    pub fn spawn<H: Handler + 'static>(self, handler: H) -> Result<ServerHandle> {
        let local_addr = self.local_addr;
        let signal = self.signal.clone();
        let engine = self.start(Arc::new(handler));
        let shared = Arc::clone(&engine.shared);
        let thread = thread::Builder::new()
            .name("belay-http-accept".into())
            .spawn(move || engine.serve())?;
        Ok(ServerHandle {
            local_addr,
            signal,
            shared,
            thread: Some(thread),
        })
    }

    fn start(self, handler: Arc<dyn Handler>) -> Engine {
        let wake = wake_address(self.local_addr);
        self.signal.on_trigger(Box::new(move || {
            // Unblocks `accept`; the loop sees the flag and exits. Failure
            // only means the listener is already gone.
            let _ = TcpStream::connect_timeout(&wake, Duration::from_secs(1));
        }));
        let shared = Arc::new(Shared {
            handler,
            shutdown: self.signal,
            counters: Counters::default(),
            config: self.config,
        });
        let conn_shared = Arc::clone(&shared);
        let pool = WorkerPool::new(
            "belay-http-conn",
            shared.config.max_connections,
            shared.config.thread_idle_timeout,
            move |(stream, peer): (TcpStream, SocketAddr)| conn::serve(stream, peer, &conn_shared),
        );
        Engine {
            listener: self.listener,
            shared,
            pool,
        }
    }
}
