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
