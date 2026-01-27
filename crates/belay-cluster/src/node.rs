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
