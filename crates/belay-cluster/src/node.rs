//! Runs the membership state machine over a transport on two threads.
//!
//! `belay-gossip-rx` blocks on the transport and feeds each datagram to the
//! state machine; `belay-gossip-tick` sleeps until the machine's next
//! deadline and ticks it. Both send what the machine returns. The machine
//! sits behind one mutex, held only while it computes (never across I/O),
//! so the two threads never wait on each other for long.
//!
//! Readers (`members`, `is_owner`, `leader`...) never touch that mutex:
//! after every change the runner publishes an immutable snapshot of the
//! member list and its ring, and readers clone an `Arc` to it. Ownership
//! checks on hot request paths are therefore a read lock and a handful of
//! hashes.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use belay_common::sync::{lock, read, write};
use belay_common::time::system_clock;
use belay_common::{debug, info, warn, Clock, Rng, SharedClock, Shutdown};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::event::MembershipEvent;
use crate::member::{Member, Meta, NodeId};
use crate::membership::{LocalState, Membership, MembershipStats, Outbound};
use crate::ring::Ring;
use crate::transport::{Datagram, Transport};

/// How long the receive thread blocks before re-checking for shutdown.
const RECV_POLL: Duration = Duration::from_millis(50);
/// Longest sleep of the tick thread, bounding how late it notices
/// deadlines created by incoming datagrams.
const MAX_TICK_SLEEP: Duration = Duration::from_millis(50);
/// Pause after a transport receive error, to avoid spinning on a broken
/// socket.
const ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// An immutable snapshot of the membership, published after each change.
#[derive(Debug)]
struct View {
    version: u64,
    state: LocalState,
    /// Every known member, in id order.
    members: Vec<Member>,
    ring: Ring,
}

impl View {
    fn of(m: &Membership) -> View {
        View {
            version: m.version(),
            state: m.state(),
            members: m.members().cloned().collect(),
            ring: m.ring(),
        }
    }

    fn member(&self, id: &str) -> Option<&Member> {
        self.members
            .binary_search_by(|m| m.id.as_str().cmp(id))
            .ok()
            .map(|i| &self.members[i])
    }
}

struct Shared {
    id: NodeId,
    gossip_addr: SocketAddr,
    leave_timeout: Duration,
    membership: Mutex<Membership>,
    transport: Arc<dyn Transport>,
    clock: SharedClock,
    view: RwLock<Arc<View>>,
    subscribers: Mutex<Vec<Sender<MembershipEvent>>>,
    stopped: AtomicBool,
}

impl Shared {
    /// Runs `f` on the state machine, then sends its output, publishes a
    /// new snapshot if anything changed and delivers events.
    fn drive<R>(&self, f: impl FnOnce(&mut Membership) -> (Vec<Outbound>, R)) -> R {
        let (out, result, events, view) = {
            let mut m = lock(&self.membership);
            let (out, result) = f(&mut m);
            let events = m.drain_events();
            let current = read(&self.view).clone();
            let view = (current.version != m.version() || current.state != m.state())
                .then(|| Arc::new(View::of(&m)));
            (out, result, events, view)
        };
        if let Some(view) = view {
            *write(&self.view) = view;
        }
        self.send_all(out);
        self.publish(events);
        result
    }

    fn send_all(&self, out: Vec<Outbound>) {
        for o in out {
            if let Err(e) = self.transport.send_to(o.to, &o.bytes) {
                debug!("gossip send failed"; to = %o.to, error = %e);
            }
        }
    }

    fn publish(&self, events: Vec<MembershipEvent>) {
        if events.is_empty() {
            return;
        }
        let mut subscribers = lock(&self.subscribers);
        subscribers.retain(|tx| events.iter().all(|e| tx.send(e.clone()).is_ok()));
    }

    fn on_datagram(&self, d: Datagram) {
        let now = self.clock.now();
        self.drive(|m| (m.handle_datagram(d.from, &d.bytes, now), ()));
    }

    /// Ticks the machine and returns how long to sleep.
    fn tick(&self) -> Duration {
        let now = self.clock.now();
        let next = self.drive(|m| {
            let out = m.tick(now);
            (out, m.next_wakeup())
        });
        next.saturating_duration_since(self.clock.now())
            .clamp(Duration::from_millis(1), MAX_TICK_SLEEP)
    }

    fn view(&self) -> Arc<View> {
        read(&self.view).clone()
    }
}

/// A running cluster member: the SWIM protocol driven by background
/// threads over a [`Transport`].
///
/// ```
/// use belay_cluster::{Config, MemNetwork, Node, NodeId};
/// use std::time::Duration;
///
/// let net = MemNetwork::new(1);
/// let a = Node::start(Config::local(NodeId::new("a").unwrap()), net.bind_next())?;
/// let b = Node::start(
///     Config::local(NodeId::new("b").unwrap()).with_seeds([a.gossip_addr()]),
///     net.bind_next(),
/// )?;
/// assert!(b.wait_until_joined(Duration::from_secs(5)));
///
/// // Both nodes agree on who owns what, and on the leader.
/// let owner = b.owner_of("queue:emails").unwrap().id;
/// assert!(a.is_owner("queue:emails") == (owner == "a"));
/// assert_eq!(b.leader().unwrap().id, "a");
///
/// b.leave();
/// a.shutdown();
/// # Ok::<(), belay_cluster::Error>(())
/// ```
pub struct Node {
    shared: Arc<Shared>,
    shutdown: Shutdown,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let view = self.shared.view();
        f.debug_struct("Node")
            .field("id", &self.shared.id)
            .field("gossip_addr", &self.shared.gossip_addr)
            .field("state", &view.state)
            .field("members", &view.members.len())
            .finish()
    }
}

impl Node {
    /// Starts a node on the system clock. If `config.advertise` is unset,
    /// the transport's local address is advertised, which must then be
    /// reachable by the other nodes (not `0.0.0.0`).
    pub fn start<T: Transport + 'static>(config: Config, transport: T) -> Result<Node> {
        Node::start_with(config, transport, system_clock(), Rng::from_entropy())
    }

    /// Starts a node with an explicit clock and random generator.
    pub fn start_with<T: Transport + 'static>(
        mut config: Config,
        transport: T,
        clock: SharedClock,
        rng: Rng,
    ) -> Result<Node> {
        let transport: Arc<dyn Transport> = Arc::new(transport);
        if config.advertise.is_none() {
            let local = transport.local_addr();
            if local.ip().is_unspecified() {
                return Err(Error::config(format!(
                    "the gossip transport is bound to {local}; set an advertise address other nodes can reach"
                )));
            }
            config.advertise = Some(local);
        }
        let leave_timeout = config.leave_timeout;
        let membership = Membership::new(config, rng, clock.now())?;
        let id = membership.local_id().clone();
        let gossip_addr = membership.local_member().gossip_addr;
        info!(
            "cluster node started";
            node = %id,
            gossip = %gossip_addr,
            seeds = membership.seeds().len()
        );
        let shared = Arc::new(Shared {
            id,
            gossip_addr,
            leave_timeout,
            view: RwLock::new(Arc::new(View::of(&membership))),
            membership: Mutex::new(membership),
            transport,
            clock,
            subscribers: Mutex::new(Vec::new()),
            stopped: AtomicBool::new(false),
        });
        let shutdown = Shutdown::new();
        let node = Node {
            shared,
            shutdown,
            threads: Mutex::new(Vec::new()),
        };
        let rx = node.spawn("belay-gossip-rx", receive_loop)?;
        lock(&node.threads).push(rx);
        let tick = node.spawn("belay-gossip-tick", tick_loop)?;
        lock(&node.threads).push(tick);
        Ok(node)
    }

    fn spawn(&self, name: &str, body: fn(&Shared, &Shutdown)) -> Result<JoinHandle<()>> {
        let shared = Arc::clone(&self.shared);
        let shutdown = self.shutdown.clone();
        let handle = thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || body(&shared, &shutdown));
        handle.map_err(|e| {
            self.shutdown();
            Error::Io(e)
        })
    }

    /// This node's id.
    #[must_use]
    pub fn id(&self) -> &NodeId {
        &self.shared.id
    }

    /// The gossip address this node advertises.
    #[must_use]
    pub fn gossip_addr(&self) -> SocketAddr {
        self.shared.gossip_addr
    }

    /// Whether the node is still running (not shut down, not left).
    #[must_use]
    pub fn is_running(&self) -> bool {
        !self.shared.stopped.load(Ordering::SeqCst)
    }

    /// The local node's lifecycle state.
    #[must_use]
    pub fn state(&self) -> LocalState {
        self.shared.view().state
    }

    /// The local node's own record.
    #[must_use]
    pub fn local_member(&self) -> Member {
        self.member(self.shared.id.as_str())
            .expect("the local member is always in the view")
    }

    /// Every known member (the local node, live members and tombstones of
    /// dead or departed ones), in id order.
    #[must_use]
    pub fn members(&self) -> Vec<Member> {
        self.shared.view().members.clone()
    }

    /// The live members, in id order.
    #[must_use]
    pub fn live_members(&self) -> Vec<Member> {
        self.shared
            .view()
            .members
            .iter()
            .filter(|m| m.is_live())
            .cloned()
            .collect()
    }

    /// The member with this id.
    #[must_use]
    pub fn member(&self, id: &str) -> Option<Member> {
        self.shared.view().member(id).cloned()
    }

    /// The ring of live members.
    #[must_use]
    pub fn ring(&self) -> Ring {
        self.shared.view().ring.clone()
    }

    /// The live member that owns `key` in this node's view.
    #[must_use]
    pub fn owner_of(&self, key: &str) -> Option<Member> {
        let view = self.shared.view();
        let id = view.ring.owner(key)?;
        view.member(id.as_str()).cloned()
    }

    /// The `n` best candidates for `key`: the owner first, then whoever
    /// would take over from it.
    #[must_use]
    pub fn owners(&self, key: &str, n: usize) -> Vec<Member> {
        let view = self.shared.view();
        view.ring
            .owners(key, n)
            .into_iter()
            .filter_map(|id| view.member(id.as_str()).cloned())
            .collect()
    }

    /// Whether this node owns `key`. Always false once stopped.
    #[must_use]
    pub fn is_owner(&self, key: &str) -> bool {
        self.is_running() && self.shared.view().ring.owner(key) == Some(&self.shared.id)
    }

    /// The live member with the lowest id (see
    /// [`Membership::leader`]).
    #[must_use]
    pub fn leader(&self) -> Option<Member> {
        self.shared
            .view()
            .members
            .iter()
            .find(|m| m.is_live())
            .cloned()
    }

    /// Whether this node is the [`leader`](Self::leader). Always false
    /// once stopped.
    #[must_use]
    pub fn is_leader(&self) -> bool {
        self.is_running() && self.leader().is_some_and(|m| m.id == self.shared.id)
    }

    /// Protocol counters.
    #[must_use]
    pub fn stats(&self) -> MembershipStats {
        lock(&self.shared.membership).stats()
    }

    /// A channel receiving every membership change from now on. The
    /// channel is unbounded; drop the receiver to unsubscribe.
    #[must_use]
    pub fn subscribe(&self) -> Receiver<MembershipEvent> {
        let (tx, rx) = mpsc::channel();
        lock(&self.shared.subscribers).push(tx);
        rx
    }

    /// Blocks until the node has joined through a seed (or needed none),
    /// up to `timeout`. Returns whether it has.
    pub fn wait_until_joined(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.state() == LocalState::Active {
                return true;
            }
            if !self.is_running() || Instant::now() >= deadline {
                return false;
            }
            self.shutdown.wait_timeout(Duration::from_millis(5));
        }
    }

    /// Contacts additional seeds, e.g. to merge with another cluster.
    pub fn join(&self, seeds: &[SocketAddr]) -> Result<()> {
        self.ensure_running()?;
        let now = self.shared.clock.now();
        self.shared.drive(|m| (m.join(seeds, now), ()));
        Ok(())
    }

    /// Publishes new metadata for this node.
    pub fn update_meta(&self, meta: Meta) -> Result<()> {
        self.ensure_running()?;
        self.shared.drive(|m| (Vec::new(), m.update_meta(meta)))
    }

    fn ensure_running(&self) -> Result<()> {
        if self.is_running() {
            Ok(())
        } else {
            Err(Error::Stopped)
        }
    }

    /// Leaves the cluster gracefully: announces the departure, keeps
    /// gossiping until the news has been sent enough times (or
    /// [`leave_timeout`](Config::leave_timeout) passes), then stops.
    /// Returns whether the announcement finished spreading; if not, the
    /// other nodes will declare this one dead instead.
    pub fn leave(&self) -> bool {
        if !self.is_running() {
            return false;
        }
        let now = self.shared.clock.now();
        self.shared.drive(|m| (m.leave(now), ()));
        let deadline = Instant::now() + self.shared.leave_timeout;
        let propagated = loop {
            if lock(&self.shared.membership).state() == LocalState::Left {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            self.shutdown.wait_timeout(Duration::from_millis(5));
        };
        if !propagated {
            warn!(
                "leave announcement did not finish spreading before the timeout";
                node = %self.shared.id,
                timeout = self.shared.leave_timeout
            );
        }
        self.shutdown();
        propagated
    }

    /// Stops the node's threads without announcing anything: to the other
    /// nodes this looks like a crash, and they will declare it dead.
    pub fn shutdown(&self) {
        if self.shared.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        self.shutdown.trigger();
        let threads = std::mem::take(&mut *lock(&self.threads));
        for handle in threads {
            if handle.join().is_err() {
                warn!("cluster thread panicked"; node = %self.shared.id);
            }
        }
        info!("cluster node stopped"; node = %self.shared.id);
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn receive_loop(shared: &Shared, shutdown: &Shutdown) {
    while !shutdown.is_triggered() {
        match shared.transport.recv_timeout(RECV_POLL) {
            Ok(Some(datagram)) => shared.on_datagram(datagram),
            Ok(None) => {}
            Err(e) => {
                warn!("gossip receive failed"; node = %shared.id, error = %e);
                shutdown.wait_timeout(ERROR_BACKOFF);
            }
        }
    }
}

fn tick_loop(shared: &Shared, shutdown: &Shutdown) {
    loop {
        let sleep = shared.tick();
        if shutdown.wait_timeout(sleep) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::member::MemberState;
    use crate::transport::MemNetwork;
    use crate::UdpTransport;
    use belay_common::log::{set_max_level, LevelFilter};
    use std::sync::Once;

    fn quiet() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| set_max_level(LevelFilter::Warn));
    }

    fn id(s: &str) -> NodeId {
        NodeId::new(s).unwrap()
    }

    fn eventually(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        cond()
    }

    #[test]
    fn refuses_to_advertise_an_unspecified_address() {
        quiet();
        let transport = UdpTransport::bind("0.0.0.0:0").unwrap();
        let err = Node::start(Config::local(id("a")), transport).unwrap_err();
        assert!(
            err.to_string().contains("set an advertise address"),
            "{err}"
        );
    }

    #[test]
    fn a_single_node_owns_everything_and_leads() {
        quiet();
        let net = MemNetwork::new(1);
        let node = Node::start(Config::local(id("solo")), net.bind_next()).unwrap();
        assert!(node.wait_until_joined(Duration::from_secs(1)));
        assert!(node.is_owner("anything"));
        assert!(node.is_leader());
        assert_eq!(node.local_member().state, MemberState::Alive);
        assert_eq!(node.live_members().len(), 1);
        node.shutdown();
        assert!(!node.is_running());
        assert!(!node.is_owner("anything"));
        assert!(!node.is_leader());
        assert!(matches!(node.update_meta(Meta::new()), Err(Error::Stopped)));
        assert!(!node.leave());
    }

    #[test]
    fn metadata_updates_reach_other_nodes() {
        quiet();
        let net = MemNetwork::new(2);
        let a = Node::start(Config::local(id("a")), net.bind_next()).unwrap();
        let b = Node::start(
            Config::local(id("b")).with_seeds([a.gossip_addr()]),
            net.bind_next(),
        )
        .unwrap();
        assert!(b.wait_until_joined(Duration::from_secs(5)));
        let events = a.subscribe();
        let meta = Meta::from([("weight".to_owned(), "3".to_owned())]);
        b.update_meta(meta).unwrap();
        let updated = std::iter::from_fn(|| events.recv_timeout(Duration::from_secs(5)).ok())
            .find(|e| matches!(e, MembershipEvent::Updated(m) if m.id == "b"))
            .expect("update event");
        assert_eq!(updated.member().weight(), 3.0);
        assert!(eventually(Duration::from_secs(5), || a.ring().weight("b") == Some(3.0)));
    }
}
