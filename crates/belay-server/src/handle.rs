//! Starting a server and stopping it in order: the HTTP layer stops
//! accepting and drains its connections, then the engine shuts down.
//!
//! A supervisor thread waits for the HTTP server to finish, however that
//! shutdown was triggered ([`ServerHandle::shutdown`], `POST
//! /v1/admin/shutdown`, dropping the handle), and then stops the engine.
//! Stopping the engine last lets requests still in flight during the
//! drain complete against a working engine: a worker reporting its result
//! while the server drains gets its answer.

use std::fmt;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use belay_common::{error, info, warn};
use belay_core::Engine;
use belay_http::{Drain, Server, ShutdownSignal};

use crate::app::App;
use crate::error::{Error, Result};
use crate::options::{http_config, ServerOptions};
use crate::routes;

/// A running server.
///
/// Dropping the handle shuts the server down and waits for it, like
/// [`shutdown_and_join`](Self::shutdown_and_join); call
/// [`wait`](Self::wait) to keep serving until a shutdown comes from
/// elsewhere.
pub struct ServerHandle {
    local_addr: SocketAddr,
    signal: ShutdownSignal,
    engine: Arc<Engine>,
    supervisor: Option<JoinHandle<()>>,
}

impl ServerHandle {
    /// The address the server listens on, with the actual port when it
    /// was bound to port 0.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The engine being served.
    #[must_use]
    pub fn engine(&self) -> &Arc<Engine> {
        &self.engine
    }

    /// Starts a graceful shutdown and returns at once: the server stops
    /// accepting connections, finishes the requests in flight (long polls
    /// and event streams end early), waits up to `server.shutdown_grace`
    /// for them, and then shuts the engine down. Idempotent.
    pub fn shutdown(&self) {
        self.signal.trigger();
    }

    /// Whether a shutdown has been started, by any means.
    #[must_use]
    pub fn is_shutting_down(&self) -> bool {
        self.signal.is_triggered()
    }

    /// Blocks until the server has shut down, whether through
    /// [`shutdown`](Self::shutdown) or `POST /v1/admin/shutdown`, and the
    /// engine has stopped.
    pub fn wait(mut self) {
        self.join();
    }

    /// [`shutdown`](Self::shutdown), then [`wait`](Self::wait).
    pub fn shutdown_and_join(self) {
        self.shutdown();
        self.wait();
    }

    fn join(&mut self) {
        if let Some(supervisor) = self.supervisor.take() {
            if supervisor.join().is_err() {
                error!("the server supervisor thread panicked");
            }
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        if self.supervisor.is_some() {
            self.signal.trigger();
            self.join();
        }
    }
}

impl fmt::Debug for ServerHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerHandle")
            .field("local_addr", &self.local_addr)
            .field("shutting_down", &self.signal.is_triggered())
            .field("node", &self.engine.node_id())
            .finish()
    }
}

/// Binds `listen`, reporting the address as written on failure.
pub(crate) fn bind(listen: &str) -> Result<TcpListener> {
    TcpListener::bind(listen).map_err(|source| Error::Bind {
        addr: listen.to_owned(),
        source,
    })
}

/// Serves `engine` on `listener` with `options`.
pub(crate) fn start(
    engine: Arc<Engine>,
    listener: TcpListener,
    options: ServerOptions,
) -> Result<ServerHandle> {
    let config = engine.config();
    let server = Server::from_listener(listener, http_config(&config.server))?;
    let local_addr = server.local_addr();
    let signal = server.shutdown_signal();
    let router = routes::build(Arc::new(App::new(Arc::clone(&engine), options)));
    let running = server.spawn(router)?;

    let stopping = Arc::clone(&engine);
    let supervisor = thread::Builder::new()
        .name("belay-server".to_owned())
        .spawn(move || {
            match running.join() {
                Drain::Complete => info!("HTTP server drained"),
                Drain::TimedOut { remaining } => warn!(
                    "shutdown grace period ran out with connections still open";
                    remaining
                ),
            }
            stopping.shutdown();
            info!("server stopped"; node = stopping.node_id());
        })
        // A failed spawn drops the closure and with it the HTTP server's
        // handle, which stops that server before the error is returned.
        .map_err(|e| Error::Http(belay_http::Error::Io(e)))?;

    info!(
        "serving the belay API";
        addr = %local_addr,
        node = engine.node_id(),
        auth = config.auth.is_enforced(),
        clustered = engine.is_clustered(),
        max_connections = config.server.max_connections,
    );
    Ok(ServerHandle {
        local_addr,
        signal,
        engine,
        supervisor: Some(supervisor),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_core::Config;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::{Duration, Instant};

    fn engine() -> Arc<Engine> {
        let engine = Engine::builder()
            .config(Config::dev())
            .background(false)
            .start()
            .expect("engine starts");
        Arc::new(engine)
    }

    fn get(addr: SocketAddr, path: &str) -> String {
        let mut stream = TcpStream::connect(addr).expect("connect");
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
        )
        .expect("write");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read");
        response
    }

    #[test]
    fn serves_until_shut_down_then_stops_the_engine() {
        let engine = engine();
        let listener = TcpListener::bind("127.0.0.1:0").expect("ephemeral port");
        let handle = start(Arc::clone(&engine), listener, ServerOptions::new()).expect("starts");
        let addr = handle.local_addr();
        assert!(get(addr, "/healthz").starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(!handle.is_shutting_down());

        let started = Instant::now();
        handle.shutdown_and_join();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(engine.is_shutting_down());
        assert!(TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_err());
    }

    #[test]
    fn dropping_the_handle_shuts_down() {
        let engine = engine();
        let listener = TcpListener::bind("127.0.0.1:0").expect("ephemeral port");
        let handle = start(Arc::clone(&engine), listener, ServerOptions::new()).expect("starts");
        assert!(format!("{handle:?}").contains("shutting_down: false"));
        drop(handle);
        assert!(engine.is_shutting_down());
    }

    #[test]
    fn bind_failures_name_the_configured_address() {
        let taken = TcpListener::bind("127.0.0.1:0").expect("ephemeral port");
        let addr = taken.local_addr().expect("bound").to_string();
        match bind(&addr) {
            Err(Error::Bind { addr: reported, .. }) => assert_eq!(reported, addr),
            other => panic!("expected a bind error, got {other:?}"),
        }
    }
}
