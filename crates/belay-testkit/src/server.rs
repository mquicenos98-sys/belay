//! [`TestServer`] and its builder: an engine behind a real HTTP server,
//! the clock it runs on, and the store kept across restarts and crashes.

use std::fmt;
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use belay_client::Client;
use belay_cluster::{Datagram, MemNetwork, MemTransport, Transport};
use belay_common::fs::TempDir;
use belay_common::sync::{read, write};
use belay_common::{debug, Clock, ManualClock, SharedClock, Timestamp};
use belay_core::config::{ApiToken, Scope};
use belay_core::{Config, Engine, EngineBuilder, TickReport};
use belay_proto::event::EventFilter;
use belay_server::{ServerHandle, ServerOptions};
use belay_store::vfs::{Crash, MemFs};
use belay_store::{LogKv, MemKv, SyncPolicy};

use crate::endpoint::{connect, Endpoint};
use crate::error::{Error, Result};
use crate::events::EventLog;
use crate::fixtures::start_time;
use crate::logs::init_test_logging;
use crate::worker::TestWorkerBuilder;

/// Token secrets installed by [`TestServerBuilder::standard_tokens`], one
/// per scope, named after their role.
pub mod tokens {
    /// Token `admin`: every scope.
    pub const ADMIN: &str = "admin-token-0123456789";
    /// Token `producer`: `enqueue`.
    pub const PRODUCER: &str = "producer-token-0123456789";
    /// Token `worker`: `work`.
    pub const WORKER: &str = "worker-token-0123456789";
    /// Token `reader`: `read`.
    pub const READER: &str = "reader-token-0123456789";
}

/// Where a [`TestServer`] keeps its data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Storage {
    /// An in-memory store that survives [`TestServer::restart`] (the kit
    /// keeps it), like a disk would. The default.
    Memory,
    /// A log-structured store in a temporary directory, opened by the
    /// engine from `[storage]` exactly as `belay server` does; removed
    /// when the server is dropped.
    Disk {
        /// When the write-ahead log is synced.
        sync: SyncPolicy,
    },
    /// A log-structured store on an in-memory file system that can
    /// simulate power loss ([`TestServer::crash`]).
    SimulatedDisk {
        /// When the write-ahead log is synced.
        sync: SyncPolicy,
    },
}

impl Storage {
    /// [`Storage::Disk`] syncing every commit.
    #[must_use]
    pub fn disk() -> Storage {
        Storage::Disk {
            sync: SyncPolicy::Always,
        }
    }

    /// [`Storage::SimulatedDisk`] syncing every commit.
    #[must_use]
    pub fn simulated_disk() -> Storage {
        Storage::SimulatedDisk {
            sync: SyncPolicy::Always,
        }
    }
}

/// The clock a harness runs on.
#[derive(Clone, Debug)]
pub enum TestClock {
    /// Moves only when the test says so.
    Manual(ManualClock),
    /// Real time.
    System,
}

impl TestClock {
    /// The current time.
    #[must_use]
    pub fn now(&self) -> Timestamp {
        match self {
            TestClock::Manual(clock) => clock.now(),
            TestClock::System => Timestamp::now(),
        }
    }

    /// The clock as the engine takes it.
    #[must_use]
    pub fn shared(&self) -> SharedClock {
        match self {
            TestClock::Manual(clock) => clock.shared(),
            TestClock::System => belay_common::system_clock(),
        }
    }

    /// The manual clock, if this is one.
    #[must_use]
    pub fn manual(&self) -> Option<&ManualClock> {
        match self {
            TestClock::Manual(clock) => Some(clock),
            TestClock::System => None,
        }
    }

    /// Moves a manual clock forward and returns the new time.
    pub fn advance(&self, d: Duration) -> Result<Timestamp> {
        Ok(self.require_manual("advancing time")?.advance(d))
    }

    /// Sets a manual clock.
    pub fn set(&self, t: Timestamp) -> Result<()> {
        self.require_manual("setting the time")?.set(t);
        Ok(())
    }

    fn require_manual(&self, what: &str) -> Result<&ManualClock> {
        self.manual()
            .ok_or_else(|| Error::Unsupported(format!("{what} needs a manual clock")))
    }
}

/// A cluster node's place on the in-memory gossip network.
#[derive(Debug)]
pub(crate) struct ClusterSeat {
    net: MemNetwork,
    gossip: SocketAddr,
    transport: Option<Arc<SeatTransport>>,
}

impl ClusterSeat {
    pub(crate) fn new(net: MemNetwork, gossip: SocketAddr) -> ClusterSeat {
        ClusterSeat {
            net,
            gossip,
            transport: None,
        }
    }

    /// Binds the node's gossip address. It was released when the node
    /// stopped; should something else hold it, the node moves to a fresh
    /// address.
    fn bind(&mut self) -> Arc<SeatTransport> {
        let deadline = Instant::now() + Duration::from_secs(1);
        let endpoint = loop {
            match self.net.bind(self.gossip) {
                Ok(transport) => break transport,
                Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Err(_) => {
                    let transport = self.net.bind_next();
                    self.gossip = transport.local_addr();
                    break transport;
                }
            }
        };
        let transport = Arc::new(SeatTransport {
            addr: self.gossip,
            endpoint: RwLock::new(Some(endpoint)),
        });
        self.transport = Some(Arc::clone(&transport));
        transport
    }

    /// Unbinds the gossip address of a stopped node. A stopped engine
    /// still holds its cluster node, and with it the transport, so the
    /// address is released here rather than when the engine is dropped.
    fn release(&mut self) {
        if let Some(transport) = self.transport.take() {
            write(&transport.endpoint).take();
        }
    }
}
