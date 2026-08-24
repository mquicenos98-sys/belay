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

/// A [`MemTransport`] the kit can unbind while the engine holding it lives
/// on.
#[derive(Debug)]
struct SeatTransport {
    addr: SocketAddr,
    endpoint: RwLock<Option<MemTransport>>,
}

impl SeatTransport {
    fn released() -> io::Error {
        io::Error::new(io::ErrorKind::NotConnected, "the node has stopped")
    }
}

impl Transport for SeatTransport {
    fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    fn send_to(&self, to: SocketAddr, bytes: &[u8]) -> io::Result<()> {
        match &*read(&self.endpoint) {
            Some(endpoint) => endpoint.send_to(to, bytes),
            None => Err(SeatTransport::released()),
        }
    }

    fn recv_timeout(&self, timeout: Duration) -> io::Result<Option<Datagram>> {
        match &*read(&self.endpoint) {
            Some(endpoint) => endpoint.recv_timeout(timeout),
            None => Err(SeatTransport::released()),
        }
    }
}

/// Builds a [`TestServer`].
#[must_use]
pub struct TestServerBuilder {
    config: Config,
    options: ServerOptions,
    clock: TestClock,
    background: Option<bool>,
    storage: Storage,
    seed: u64,
    cluster: Option<ClusterSeat>,
}

impl TestServerBuilder {
    fn new() -> TestServerBuilder {
        let mut config = Config::dev();
        config.server.shutdown_grace = Duration::from_secs(5);
        config.metrics.process = false;
        let options = ServerOptions {
            // Requests are only limited while tokens are in use; tests that
            // hammer the API with one token should not be throttled.
            rate_limit: None,
            event_heartbeat: Duration::from_millis(200),
            event_retry: Duration::from_millis(50),
            shutdown_poll: Duration::from_millis(20),
            ..ServerOptions::default()
        };
        TestServerBuilder {
            config,
            options,
            clock: TestClock::Manual(ManualClock::new(start_time())),
            background: None,
            storage: Storage::Memory,
            seed: 7,
            cluster: None,
        }
    }

    /// Replaces the whole configuration (default: [`Config::dev`] with a
    /// 5 s shutdown grace). The storage and cluster sections are
    /// overridden by [`storage`](Self::storage) and by clusters.
    pub fn config(mut self, config: Config) -> TestServerBuilder {
        self.config = config;
        self
    }

    /// Adjusts the configuration: queues, schedules, retention, ...
    pub fn configure(mut self, f: impl FnOnce(&mut Config)) -> TestServerBuilder {
        f(&mut self.config);
        self
    }

    /// Adds a bearer token; authentication is enforced from the first one.
    pub fn token(mut self, name: &str, secret: &str, scopes: &[Scope]) -> TestServerBuilder {
        let scopes = scopes.iter().copied().collect();
        self.config
            .auth
            .tokens
            .push(ApiToken::new(name, secret, scopes));
        self
    }

    /// Adds the tokens in [`tokens`]: `admin`, `producer` (enqueue),
    /// `worker` (work) and `reader` (read).
    pub fn standard_tokens(self) -> TestServerBuilder {
        self.token("admin", tokens::ADMIN, &[Scope::Admin])
            .token("producer", tokens::PRODUCER, &[Scope::Enqueue])
            .token("worker", tokens::WORKER, &[Scope::Work])
            .token("reader", tokens::READER, &[Scope::Read])
    }

    /// A manual clock starting at `start` (default
    /// [`START`](crate::fixtures::START)).
    pub fn starting_at(mut self, start: Timestamp) -> TestServerBuilder {
        self.clock = TestClock::Manual(ManualClock::new(start));
        self
    }

    /// Shares `clock` with other harnesses (a cluster's nodes, say).
    pub fn clock(mut self, clock: TestClock) -> TestServerBuilder {
        self.clock = clock;
        self
    }

    /// Real time, with background threads unless
    /// [`background`](Self::background) says otherwise.
    pub fn system_clock(mut self) -> TestServerBuilder {
        self.clock = TestClock::System;
        self
    }

    /// Whether the engine runs its background threads (default: only on
    /// the system clock).
    pub fn background(mut self, on: bool) -> TestServerBuilder {
        self.background = Some(on);
        self
    }

    /// Where data lives (default [`Storage::Memory`]).
    pub fn storage(mut self, storage: Storage) -> TestServerBuilder {
        self.storage = storage;
        self
    }

    /// Server options (default: no per-token rate limit, 200 ms event
    /// heartbeats, 50 ms reconnect hint, 20 ms shutdown polling).
    pub fn options(mut self, options: ServerOptions) -> TestServerBuilder {
        self.options = options;
        self
    }

    /// Adjusts the server options, keeping the kit's other defaults.
    pub fn configure_options(mut self, f: impl FnOnce(&mut ServerOptions)) -> TestServerBuilder {
        f(&mut self.options);
        self
    }

    /// Seeds the engine's lease tokens and retry jitter (default 7).
    pub fn seed(mut self, seed: u64) -> TestServerBuilder {
        self.seed = seed;
        self
    }

    pub(crate) fn cluster_seat(mut self, seat: ClusterSeat) -> TestServerBuilder {
        self.cluster = Some(seat);
        self
    }

    /// Opens the store, starts the engine and serves it.
    pub fn start(self) -> Result<TestServer> {
        init_test_logging();
        let background = self
            .background
            .unwrap_or(matches!(self.clock, TestClock::System));
        let mut setup = Setup {
            config: self.config,
            options: self.options,
            clock: self.clock,
            background,
            seed: self.seed,
            cluster: self.cluster,
        };
        let mut store = Store::create(self.storage)?;
        let running = launch(&mut setup, &mut store, 0)?;
        Ok(TestServer {
            endpoint: Endpoint::new(format!("http://{}", running.addr)),
            setup,
            store,
            engine: running.engine,
            handle: Some(running.handle),
            addr: running.addr,
        })
    }
}

impl fmt::Debug for TestServerBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestServerBuilder")
            .field("clock", &self.clock)
            .field("background", &self.background)
            .field("storage", &self.storage)
            .field("clustered", &self.cluster.is_some())
            .finish_non_exhaustive()
    }
}

/// What a (re)start needs besides the store.
struct Setup {
    config: Config,
    options: ServerOptions,
    clock: TestClock,
    background: bool,
    seed: u64,
    cluster: Option<ClusterSeat>,
}

/// The store, held by the kit so that it outlives the engine.
enum Store {
    Memory(Arc<MemKv>),
    Disk {
        dir: TempDir,
        sync: SyncPolicy,
    },
    Simulated {
        fs: MemFs,
        sync: SyncPolicy,
        /// Open while the engine runs; the engine does not close stores
        /// it was handed.
        kv: Option<Arc<LogKv>>,
    },
}

/// Where the simulated disk keeps the store.
const SIMULATED_DIR: &str = "belay-data";

impl Store {
    fn create(storage: Storage) -> Result<Store> {
        Ok(match storage {
            Storage::Memory => Store::Memory(Arc::new(MemKv::new())),
            Storage::Disk { sync } => Store::Disk {
                dir: TempDir::with_prefix("belay-testkit")?,
                sync,
            },
            Storage::SimulatedDisk { sync } => Store::Simulated {
                fs: MemFs::new(),
                sync,
                kv: None,
            },
        })
    }

    /// Points the engine at the store: a handed-over store, or the
    /// `[storage]` section for a real directory.
    fn attach(&mut self, builder: EngineBuilder, config: &mut Config) -> Result<EngineBuilder> {
        match self {
            Store::Memory(kv) => Ok(builder.kv(kv.clone())),
            Store::Disk { dir, sync } => {
                config.storage.in_memory = false;
                config.storage.dir = dir.join("data");
                config.storage.sync = *sync;
                Ok(builder)
            }
            Store::Simulated { fs, sync, kv } => {
                let options = belay_store::Options::new().sync(*sync).fs(fs.clone());
                let opened = Arc::new(LogKv::open(SIMULATED_DIR, options)?);
                *kv = Some(Arc::clone(&opened));
                Ok(builder.kv(opened))
            }
        }
    }

    /// Closes what the kit opened, after the engine has stopped.
    fn release(&mut self) {
        if let Store::Simulated { kv, .. } = self {
            if let Some(kv) = kv.take() {
                // After a simulated crash the handle's files are gone and
                // closing fails; that is the point of the crash.
                if let Err(e) = kv.close() {
                    debug!("closing the simulated store failed"; error = %e);
                }
            }
        }
    }
}

struct Running {
    engine: Arc<Engine>,
    handle: ServerHandle,
    addr: SocketAddr,
}

/// Binds a listener (on `port`, or any port for 0), opens the store,
/// starts the engine and serves it.
fn launch(setup: &mut Setup, store: &mut Store, port: u16) -> Result<Running> {
    let listener = bind_listener(port)?;
    let addr = listener.local_addr()?;
    let mut config = setup.config.clone();
    let mut builder = Engine::builder()
        .clock(setup.clock.shared())
        .background(setup.background)
        .seed(setup.seed);
    if let Some(seat) = &mut setup.cluster {
        // The API address is gossiped, so it must be known before the
        // node starts: that is why the listener is bound first.
        config.cluster.enabled = true;
        config.cluster.api_addr = Some(format!("http://{addr}"));
        builder = builder.cluster_transport(seat.bind());
    }
    builder = store.attach(builder, &mut config)?;
    let engine = match builder.config(config).start() {
        Ok(engine) => Arc::new(engine),
        Err(e) => {
            store.release();
            return Err(e.into());
        }
    };
    let handle = belay_server::serve_on_with(Arc::clone(&engine), listener, setup.options.clone())?;
    Ok(Running {
        engine,
        handle,
        addr,
    })
}

/// Binds `127.0.0.1:port`. A restarted server asks for its old port, which
/// the previous listener released a moment ago; if it stays taken, the
/// server moves to a fresh one.
fn bind_listener(port: u16) -> Result<TcpListener> {
    if port == 0 {
        return Ok(TcpListener::bind(("127.0.0.1", 0))?);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => return Ok(listener),
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Err(e) => {
                debug!("the old port stays taken, moving"; port, error = %e);
                return Ok(TcpListener::bind(("127.0.0.1", 0))?);
            }
        }
    }
}

/// A belay engine and its HTTP server on `127.0.0.1:0`, in this process,
/// with a clock the test controls and a store that outlives restarts.
///
/// By default the clock is a [`ManualClock`] at
/// [`START`](crate::fixtures::START) and the engine runs no background
/// threads: nothing happens by itself, and [`tick`](TestServer::tick)
/// moves time and runs one round of the periodic duties (promotions, lease
/// expiry, schedules, workflow chores). Workers still lease and report in
/// real time, so a test should let running jobs finish before jumping the
/// clock past their lease or timeout. [`TestServerBuilder::system_clock`]
/// switches to real time with the background threads on.
///
/// [`Storage`] decides where jobs live: in memory (kept across
/// [`restart`](TestServer::restart)), in a log-structured store in a
/// temporary directory, or in a log-structured store on a simulated disk
/// that can lose power ([`crash`](TestServer::crash)).
///
/// ```
/// use belay_testkit::{fixtures::job, wait_for_state, TestServer, DEFAULT_TIMEOUT};
/// use belay_proto::JobState;
///
/// let server = TestServer::start()?;
/// let worker = server.worker().queue("emails").recording("send").start()?;
/// let client = server.client();
/// let id = client.jobs().enqueue(&job("emails", "send"))?.id;
/// let done = wait_for_state(&client, id, JobState::Succeeded, DEFAULT_TIMEOUT)?;
/// assert_eq!(done.attempt, 1);
/// assert_eq!(worker.recorder().of_job(id).len(), 1);
/// # Ok::<(), belay_testkit::Error>(())
/// ```
pub struct TestServer {
    setup: Setup,
    store: Store,
    endpoint: Endpoint,
    /// The current engine; after [`stop`](TestServer::stop) (or a failed
    /// restart) a stopped one, whose calls answer `unavailable`.
    engine: Arc<Engine>,
    handle: Option<ServerHandle>,
    addr: SocketAddr,
}

impl TestServer {
    /// A server with every default: manual clock, no background threads,
    /// in-memory store, no authentication.
    pub fn start() -> Result<TestServer> {
        TestServer::builder().start()
    }

    /// A builder for anything else.
    pub fn builder() -> TestServerBuilder {
        TestServerBuilder::new()
    }

    /// The API's base URL, `http://127.0.0.1:<port>`.
    #[must_use]
    pub fn url(&self) -> String {
        self.endpoint.url()
    }

    /// The address the server listens on.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// A typed client without a token. Take a new one after a restart:
    /// pooled connections do not survive it.
    #[must_use]
    pub fn client(&self) -> Client {
        connect(&self.url(), None).expect("the kit's own URL and settings form a valid client")
    }

    /// A typed client presenting `token`.
    pub fn client_with_token(&self, token: &str) -> Result<Client> {
        connect(&self.url(), Some(token))
    }

    /// The engine behind the server, for looking under the hood.
    #[must_use]
    pub fn engine(&self) -> &Arc<Engine> {
        &self.engine
    }

    /// The configuration the engine runs with.
    #[must_use]
    pub fn config(&self) -> &Config {
        self.engine.config()
    }

    /// The harness clock.
    #[must_use]
    pub fn clock(&self) -> &TestClock {
        &self.setup.clock
    }

    /// The current time by the harness clock.
    #[must_use]
    pub fn now(&self) -> Timestamp {
        self.setup.clock.now()
    }

    /// Advances the manual clock by `d`, then runs one round of the
    /// engine's periodic duties.
    pub fn tick(&self, d: Duration) -> Result<TickReport> {
        let now = self.setup.clock.advance(d)?;
        Ok(self.engine.tick(now))
    }

    /// Sets the manual clock to `t`, then runs one round of duties.
    pub fn tick_at(&self, t: Timestamp) -> Result<TickReport> {
        self.setup.clock.set(t)?;
        Ok(self.engine.tick(t))
    }

    /// Runs one round of duties at the current time, on any clock.
    #[must_use]
    pub fn tick_now(&self) -> TickReport {
        self.engine.tick(self.now())
    }

    /// A worker builder talking to this server; it follows the server
    /// across restarts.
    pub fn worker(&self) -> TestWorkerBuilder {
        TestWorkerBuilder::new(self.endpoint.clone())
    }

    /// Follows the server's event stream over HTTP from its oldest
    /// buffered event.
    pub fn event_log(&self) -> Result<EventLog> {
        EventLog::start(&self.client(), &EventFilter::new())
    }

    /// Whether the server is serving (not stopped).
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.handle.is_some()
    }

    /// The store directory, for [`Storage::Disk`].
    #[must_use]
    pub fn data_dir(&self) -> Option<PathBuf> {
        match &self.store {
            Store::Disk { dir, .. } => Some(dir.join("data")),
            _ => None,
        }
    }

    /// The simulated disk, for [`Storage::SimulatedDisk`].
    #[must_use]
    pub fn simulated_disk(&self) -> Option<&MemFs> {
        match &self.store {
            Store::Simulated { fs, .. } => Some(fs),
            _ => None,
        }
    }

    /// Stops gracefully: the server drains, the engine shuts down, the
    /// store is closed. [`restart`](Self::restart) brings it back.
    pub fn stop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.shutdown_and_join();
        }
        self.engine.shutdown();
        self.store.release();
        if let Some(seat) = &mut self.setup.cluster {
            seat.release();
        }
    }

    /// Stops (if running) and starts again on the same store and, when it
    /// is still free, the same port.
    pub fn restart(&mut self) -> Result<()> {
        self.stop();
        self.relaunch()
    }

    /// Stops, moves the manual clock forward by `downtime`, and starts
    /// again: what the engine sees after being down that long.
    pub fn restart_after(&mut self, downtime: Duration) -> Result<()> {
        self.setup
            .clock
            .require_manual("a restart after downtime")?;
        self.stop();
        self.setup.clock.advance(downtime)?;
        self.relaunch()
    }

    /// Cuts the power ([`Crash::PowerLoss`]): only what the store synced
    /// survives. Then tears the dead server down and starts again on what
    /// is left. Needs [`Storage::SimulatedDisk`].
    pub fn crash(&mut self) -> Result<()> {
        self.crash_with(&Crash::PowerLoss)
    }

    /// [`crash`](Self::crash) with another kind of crash.
    pub fn crash_with(&mut self, crash: &Crash) -> Result<()> {
        let Store::Simulated { fs, .. } = &self.store else {
            return Err(Error::Unsupported(
                "crashes can only be simulated on Storage::SimulatedDisk".to_owned(),
            ));
        };
        // First the crash, at this instant; shutting down afterwards cannot
        // sync anything any more, as the old handles are dead.
        fs.crash(crash);
        self.stop();
        self.relaunch()
    }

    pub(crate) fn set_seeds(&mut self, seeds: Vec<String>) {
        self.setup.config.cluster.seeds = seeds;
    }

    pub(crate) fn gossip_addr(&self) -> Option<SocketAddr> {
        self.setup.cluster.as_ref().map(|seat| seat.gossip)
    }

    fn relaunch(&mut self) -> Result<()> {
        let running = launch(&mut self.setup, &mut self.store, self.addr.port())?;
        if running.addr != self.addr {
            self.endpoint.set(format!("http://{}", running.addr));
            self.addr = running.addr;
        }
        self.engine = running.engine;
        self.handle = Some(running.handle);
        Ok(())
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop();
    }
}

impl fmt::Debug for TestServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestServer")
            .field("url", &self.url())
            .field("node", &self.engine.node_id())
            .field("running", &self.is_running())
            .field("clock", &self.setup.clock)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_proto::JobSpec;
    use std::path::Path;

    fn file_names(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn a_manual_clock_moves_only_when_told() {
        let server = TestServer::start().unwrap();
        let before = server.now();
        assert_eq!(before, start_time());
        let report = server.tick(Duration::from_secs(90)).unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(server.now(), before + Duration::from_secs(90));
        assert_eq!(server.engine().info().time, server.now());
    }

    #[test]
    fn the_system_clock_cannot_be_advanced() {
        let clock = TestClock::System;
        assert!(matches!(
            clock.advance(Duration::from_secs(1)),
            Err(Error::Unsupported(_))
        ));
        assert!(clock.manual().is_none());
    }

    #[test]
    fn memory_stores_survive_a_restart_on_the_same_port() {
        let mut server = TestServer::start().unwrap();
        let id = server
            .client()
            .jobs()
            .enqueue(&JobSpec::new("emails", "send"))
            .unwrap()
            .id;
        let url = server.url();
        server.restart().unwrap();
        assert!(server.is_running());
        assert_eq!(server.client().jobs().get(id).unwrap().id, id);
        // Ports are reused when free, which they are right after a stop.
        assert_eq!(server.url(), url);
    }

    #[test]
    fn crashing_needs_a_simulated_disk() {
        let mut server = TestServer::start().unwrap();
        let err = server.crash().unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)), "{err}");
        assert!(server.is_running());
    }

    #[test]
    fn a_stopped_server_answers_nothing() {
        let mut server = TestServer::start().unwrap();
        server.stop();
        assert!(!server.is_running());
        assert!(server.client().health().is_err());
        assert!(server.engine().is_shutting_down());
    }

    #[test]
    fn disk_stores_live_in_a_temporary_directory() {
        let server = TestServer::builder()
            .storage(Storage::disk())
            .start()
            .unwrap();
        let dir = server.data_dir().expect("a disk store has a directory");
        assert!(file_names(&dir).iter().any(|f| f == "LOCK"));
        assert_eq!(server.engine().store_report().storage, "disk");
    }
}
