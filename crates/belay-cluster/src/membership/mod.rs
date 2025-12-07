//! The SWIM membership state machine.
//!
//! [`Membership`] does no I/O and reads no clock: the caller feeds it
//! received datagrams ([`handle`](Membership::handle) /
//! [`handle_datagram`](Membership::handle_datagram)) and calls
//! [`tick`](Membership::tick) at least by [`next_wakeup`](Membership::next_wakeup),
//! passing the current time each time, and sends the [`Outbound`] datagrams
//! each call returns. Randomness comes from the [`Rng`] it was built with.
//! Given the same seed, inputs and times it produces the same outputs,
//! which is what makes whole-cluster simulations reproducible.
//!
//! The work is split by concern: `probe` (failure detection), `merge`
//! (applying updates, suspicion, refutation) and `sync` (join,
//! anti-entropy, reconnection).

mod merge;
mod probe;
mod sync;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

use belay_common::{debug, info, warn, Rng, Timestamp};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::event::MembershipEvent;
use crate::gossip::GossipQueue;
use crate::member::{validate_meta, Member, MemberState, Meta, NodeId};
use crate::ring::Ring;
use crate::suspicion::Suspicion;
use crate::wire::{self, FrameWriter, Message, Packet, Update};

use self::probe::{Probe, Relay};

/// A datagram to send.
#[derive(Clone, PartialEq, Eq)]
pub struct Outbound {
    /// Destination gossip address.
    pub to: SocketAddr,
    /// The encoded datagram, at most
    /// [`max_packet_size`](Config::max_packet_size) bytes.
    pub bytes: Vec<u8>,
}

impl fmt::Debug for Outbound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Outbound")
            .field("to", &self.to)
            .field("len", &self.bytes.len())
            .finish()
    }
}

/// The local node's own progress through the cluster lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LocalState {
    /// Seeds are configured and none has answered yet.
    Joining,
    /// Part of the cluster (or the first node of one).
    Active,
    /// [`leave`](Membership::leave) was called; still spreading the news.
    Leaving,
    /// Gone: the state machine ignores all input.
    Left,
}

impl LocalState {
    /// Lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            LocalState::Joining => "joining",
            LocalState::Active => "active",
            LocalState::Leaving => "leaving",
            LocalState::Left => "left",
        }
    }
}

impl fmt::Display for LocalState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// Counters describing the protocol's activity since start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MembershipStats {
    /// Datagrams accepted.
    pub packets_received: u64,
    /// Datagrams dropped as malformed, from another cluster or version.
    pub packets_rejected: u64,
    /// Datagrams produced.
    pub packets_sent: u64,
    /// Probes started (one per protocol period with someone to probe).
    pub probes: u64,
    /// Probes that fell back to indirect probing.
    pub indirect_probes: u64,
    /// Probes that got no ack at all within their period.
    pub probe_failures: u64,
    /// Members this node moved to suspect (on its own or through gossip).
    pub suspicions: u64,
    /// Times this node refuted a claim that it was suspect, dead or gone.
    pub refutations: u64,
    /// Full-state exchanges this node initiated.
    pub syncs_sent: u64,
    /// Full-state datagrams received.
    pub syncs_received: u64,
}

/// Upcoming deadlines of the periodic duties.
#[derive(Clone, Copy, Debug)]
struct Timers {
    probe: Timestamp,
    gossip: Timestamp,
    push_pull: Timestamp,
    reconnect: Timestamp,
    join: Timestamp,
    reap: Timestamp,
}

/// How often tombstones are checked for expiry.
const REAP_CHECK: Duration = Duration::from_secs(1);
/// How often a node using this node's id is reported.
const CONFLICT_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// One node's view of the cluster, maintained by the SWIM protocol.
///
/// See the [crate documentation](crate) for the protocol.
///
/// ```
/// use belay_cluster::{Config, Membership, NodeId};
/// use belay_common::{Rng, Timestamp};
///
/// let t0 = Timestamp::from_secs(1_000);
/// let config = |id: &str, port: u16| {
///     Config::new(NodeId::new(id).unwrap())
///         .with_advertise(format!("10.0.0.1:{port}").parse().unwrap())
/// };
/// let mut a = Membership::new(config("a", 7001), Rng::seed_from_u64(1), t0).unwrap();
/// let mut b = Membership::new(
///     config("b", 7002).with_seeds(["10.0.0.1:7001".parse().unwrap()]),
///     Rng::seed_from_u64(2),
///     t0,
/// )
/// .unwrap();
///
/// // Deliver datagrams by hand until both sides are quiet.
/// let a_addr = a.local_member().gossip_addr;
/// let b_addr = b.local_member().gossip_addr;
/// let mut to_a = Vec::new();
/// let mut to_b = Vec::new();
/// for out in b.tick(t0) {
///     assert_eq!(out.to, a_addr);
///     to_a.push(out.bytes);
/// }
/// while !to_a.is_empty() || !to_b.is_empty() {
///     for bytes in to_a.drain(..).collect::<Vec<_>>() {
///         to_b.extend(a.handle_datagram(b_addr, &bytes, t0).into_iter().map(|o| o.bytes));
///     }
///     for bytes in to_b.drain(..).collect::<Vec<_>>() {
///         to_a.extend(b.handle_datagram(a_addr, &bytes, t0).into_iter().map(|o| o.bytes));
///     }
/// }
/// assert_eq!(a.live_count(), 2);
/// assert_eq!(b.live_count(), 2);
/// assert_eq!(a.leader().unwrap().id, "a");
/// ```
pub struct Membership {
    config: Config,
    cluster: u32,
    local: NodeId,
    /// Every known member, the local node included, in id order.
    members: BTreeMap<NodeId, Member>,
    state: LocalState,
    rng: Rng,
    seq: u64,
    probe: Option<Probe>,
    /// Randomized round-robin order for probing; reshuffled every round.
    probe_list: Vec<NodeId>,
    probe_index: usize,
    /// Pings sent on behalf of a ping request, by our seq.
    relays: BTreeMap<u64, Relay>,
    suspicions: BTreeMap<NodeId, Suspicion>,
    gossip: GossipQueue,
    seeds: Vec<SocketAddr>,
    timers: Timers,
    /// When each dead member that contacted us was last told it is dead.
    dead_notices: HashMap<NodeId, Timestamp>,
    conflict_logged_at: Option<Timestamp>,
    events: Vec<MembershipEvent>,
    version: u64,
    stats: MembershipStats,
}

impl fmt::Debug for Membership {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Membership")
            .field("local", &self.local)
            .field("state", &self.state)
            .field("members", &self.members.len())
            .field("live", &self.live_count())
            .field("pending_updates", &self.gossip.len())
            .finish_non_exhaustive()
    }
}

impl Membership {
    /// Creates the state machine for the node described by `config`, whose
    /// [`advertise`](Config::advertise) address must be set.
    ///
    /// With seeds configured the node starts [`Joining`](LocalState::Joining)
    /// and contacts them on the first [`tick`](Self::tick).
    pub fn new(config: Config, mut rng: Rng, now: Timestamp) -> Result<Membership> {
        config.validate()?;
        let gossip_addr = config.advertise.ok_or_else(|| {
            Error::config(
                "advertise address is not set (the node runner fills it in from the transport)",
            )
        })?;
        let local = config.node_id.clone();
        let me = Member {
            id: local.clone(),
            addr: config.api_addr.clone(),
            gossip_addr,
            state: MemberState::Alive,
            incarnation: config.incarnation,
            meta: config.meta.clone(),
            since: now,
        };
        let mut seeds: Vec<SocketAddr> = Vec::new();
        for seed in &config.seeds {
            if *seed != gossip_addr && !seeds.contains(seed) {
                seeds.push(*seed);
            }
        }
        // Staggered first deadlines keep nodes that start together from
        // probing and gossiping in lockstep.
        let timers = Timers {
            probe: now + stagger(&mut rng, config.probe_interval),
            gossip: now + stagger(&mut rng, config.gossip_interval),
            push_pull: enabled_after(now, config.push_pull_interval, &mut rng),
            reconnect: enabled_after(now, config.reconnect_interval, &mut rng),
            join: now,
            reap: now + REAP_CHECK,
        };
        let state = if seeds.is_empty() {
            LocalState::Active
        } else {
            LocalState::Joining
        };
        let seq = u64::from(rng.next_u32());
        Ok(Membership {
            cluster: config.cluster_tag(),
            members: BTreeMap::from([(local.clone(), me)]),
            local,
            config,
            state,
            rng,
            seq,
            probe: None,
            probe_list: Vec::new(),
            probe_index: 0,
            relays: BTreeMap::new(),
            suspicions: BTreeMap::new(),
            gossip: GossipQueue::new(),
            seeds,
            timers,
            dead_notices: HashMap::new(),
            conflict_logged_at: None,
            events: Vec::new(),
            version: 0,
            stats: MembershipStats::default(),
        })
    }

    /// The local node's id.
    #[must_use]
    pub fn local_id(&self) -> &NodeId {
        &self.local
    }

    /// The local node's own record.
    #[must_use]
    pub fn local_member(&self) -> &Member {
        self.members
            .get(&self.local)
            .expect("the local member is inserted at creation and never removed")
    }

    fn local_member_mut(&mut self) -> &mut Member {
        self.members
            .get_mut(&self.local)
            .expect("the local member is inserted at creation and never removed")
    }

    /// The local node's current incarnation.
    #[must_use]
    pub fn incarnation(&self) -> u64 {
        self.local_member().incarnation
    }

    /// The configuration in use.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Where the local node is in its lifecycle.
    #[must_use]
    pub fn state(&self) -> LocalState {
        self.state
    }

    /// Whether the node has reached a seed (or needed none).
    #[must_use]
    pub fn is_joined(&self) -> bool {
        self.state == LocalState::Active
    }

    /// The seed addresses in use.
    #[must_use]
    pub fn seeds(&self) -> &[SocketAddr] {
        &self.seeds
    }

    /// The member with this id, including dead and departed members until
    /// their tombstones expire.
    #[must_use]
    pub fn member(&self, id: &str) -> Option<&Member> {
        self.members.get(id)
    }

    /// Every known member, the local node and tombstones included, in id
    /// order.
    pub fn members(&self) -> impl Iterator<Item = &Member> + '_ {
        self.members.values()
    }

    /// The live (`Alive` or `Suspect`) members, in id order.
    pub fn live_members(&self) -> impl Iterator<Item = &Member> + '_ {
        self.members.values().filter(|m| m.is_live())
    }

    /// Number of live members, the local node included (unless leaving).
    #[must_use]
    pub fn live_count(&self) -> usize {
        self.live_members().count()
    }

    fn live_peer_count(&self) -> usize {
        self.live_members().filter(|m| m.id != self.local).count()
    }

    /// The live member with the lowest id: a cheap, coordination-free choice
    /// of one node for cluster-wide chores. Two nodes can both believe they
    /// are the leader while their views differ (e.g. during a partition),
    /// so chores must tolerate running twice.
    #[must_use]
    pub fn leader(&self) -> Option<&Member> {
        self.live_members().next()
    }

    /// A rendezvous ring of the live members.
    #[must_use]
    pub fn ring(&self) -> Ring {
        Ring::from_members(self.members.values())
    }

    /// Incremented on every change to the member list (states,
    /// incarnations, addresses, metadata); lets callers cache derived data
    /// such as the ring.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Protocol counters.
    #[must_use]
    pub fn stats(&self) -> MembershipStats {
        self.stats
    }

    /// Number of updates waiting in the gossip queue.
    #[must_use]
    pub fn pending_updates(&self) -> usize {
        self.gossip.len()
    }

    /// Takes the events produced since the last call.
    pub fn drain_events(&mut self) -> Vec<MembershipEvent> {
        std::mem::take(&mut self.events)
    }

    /// The latest time by which [`tick`](Self::tick) must be called again.
    /// Calling it earlier is harmless.
    #[must_use]
    pub fn next_wakeup(&self) -> Timestamp {
        match self.state {
            LocalState::Left => return Timestamp::MAX,
            LocalState::Leaving => return self.timers.gossip,
            LocalState::Joining | LocalState::Active => {}
        }
        let t = &self.timers;
        let mut next = [t.probe, t.gossip, t.push_pull, t.reconnect, t.reap]
            .into_iter()
            .min()
            .unwrap_or(Timestamp::MAX);
        if !self.seeds.is_empty() {
            next = next.min(t.join);
        }
        if let Some(probe) = &self.probe {
            next = next.min(probe.next_deadline());
        }
        if let Some(s) = self.suspicions.values().map(Suspicion::deadline).min() {
            next = next.min(s);
        }
        if let Some(r) = self.relays.values().map(|r| r.expires).min() {
            next = next.min(r);
        }
        next
    }

    /// Runs whatever is due at `now`: probe steps, suspicion timeouts,
    /// gossip rounds, join retries, anti-entropy, reconnection attempts and
    /// tombstone expiry.
    pub fn tick(&mut self, now: Timestamp) -> Vec<Outbound> {
        let mut out = Vec::new();
        match self.state {
            LocalState::Left => return out,
            LocalState::Leaving => {
                self.tick_gossip(now, &mut out);
                if !self.gossip.contains(self.local.as_str()) || self.live_peer_count() == 0 {
                    self.state = LocalState::Left;
                    info!("left cluster"; node = %self.local, incarnation = self.incarnation());
                }
                return out;
            }
            LocalState::Joining | LocalState::Active => {}
        }
        self.expire_relays(now);
        self.tick_probe(now, &mut out);
        self.tick_suspicions(now);
        self.tick_gossip(now, &mut out);
        self.tick_join(now, &mut out);
        self.tick_push_pull(now, &mut out);
        self.tick_reconnect(now, &mut out);
        self.tick_reap(now);
        out
    }

    /// Decodes and handles one received datagram. Malformed datagrams are
    /// counted in [`MembershipStats::packets_rejected`] and dropped.
    pub fn handle_datagram(
        &mut self,
        from: SocketAddr,
        bytes: &[u8],
        now: Timestamp,
    ) -> Vec<Outbound> {
        if self.state == LocalState::Left {
            return Vec::new();
        }
        match wire::decode(bytes, self.cluster) {
            Ok(packet) => self.handle(from, packet, now),
            Err(e) => {
                self.stats.packets_rejected += 1;
                debug!("dropped malformed packet"; from = %from, error = %e);
                Vec::new()
            }
        }
    }

    /// Handles one received packet: applies its piggybacked updates, then
    /// its message.
    pub fn handle(&mut self, from: SocketAddr, packet: Packet, now: Timestamp) -> Vec<Outbound> {
        let mut out = Vec::new();
        if self.state == LocalState::Left {
            return out;
        }
        if packet.sender == self.local {
            self.on_packet_from_own_id(from, &packet, now);
            return out;
        }
        self.stats.packets_received += 1;
        for update in packet.updates {
            self.apply_update(update, now);
        }
        self.notify_if_dead(&packet.sender, from, now, &mut out);
        match packet.message {
            Message::Ping { seq, target } => self.on_ping(from, seq, &target, &mut out),
            Message::Ack { seq } => self.on_ack(seq, &mut out),
            Message::PingReq {
                seq,
                target,
                target_addr,
            } => self.on_ping_req(from, seq, target, target_addr, now, &mut out),
            Message::Sync { reply, members } => {
                self.on_sync(from, &packet.sender, reply, members, now, &mut out);
            }
            Message::Gossip => {}
        }
        out
    }

    /// A packet carrying our own id either looped back (a seed list that
    /// names this node, possibly under another address such as a loopback
    /// one) or comes from another node configured with the same id, which
    /// breaks the protocol for both. Our own join request is recognisable
    /// by carrying exactly our current record.
    fn on_packet_from_own_id(&mut self, from: SocketAddr, packet: &Packet, now: Timestamp) {
        let me = self.local_member();
        let echo = from == me.gossip_addr
            || matches!(&packet.message, Message::Sync { members, .. }
                if members.iter().any(|r| r.id == me.id
                    && r.incarnation == me.incarnation
                    && r.gossip_addr == me.gossip_addr));
        if echo {
            return;
        }
        let due = self
            .conflict_logged_at
            .map_or(true, |at| now >= at + CONFLICT_LOG_INTERVAL);
        if due {
            self.conflict_logged_at = Some(now);
            warn!(
                "another node is using this node's id";
                node = %self.local,
                other = %from
            );
        }
    }

    /// Contacts `seeds` (in addition to the configured ones) with a full
    /// state exchange. Joining through a member of another cluster with the
    /// same name merges the two.
    pub fn join(&mut self, seeds: &[SocketAddr], now: Timestamp) -> Vec<Outbound> {
        let mut out = Vec::new();
        if matches!(self.state, LocalState::Leaving | LocalState::Left) {
            return out;
        }
        let own = self.local_member().gossip_addr;
        for &seed in seeds.iter().filter(|s| **s != own) {
            if !self.seeds.contains(&seed) {
                self.seeds.push(seed);
            }
            self.send_sync(&mut out, seed, true);
        }
        self.timers.join = now + self.config.join_retry_interval;
        out
    }

    /// Starts a graceful departure: the node marks itself `Left`, stops
    /// probing and gossips the news until every update about it has used up
    /// its retransmissions, at which point [`state`](Self::state) becomes
    /// [`Left`](LocalState::Left). Keep delivering datagrams and ticking
    /// until then (the [`Node`](crate::Node) runner does, up to
    /// [`leave_timeout`](Config::leave_timeout)).
    pub fn leave(&mut self, now: Timestamp) -> Vec<Outbound> {
        let mut out = Vec::new();
        if matches!(self.state, LocalState::Leaving | LocalState::Left) {
            return out;
        }
        let me = self.local_member_mut();
        me.state = MemberState::Left;
        me.since = now;
        let update = Update::Left {
            id: me.id.clone(),
            incarnation: me.incarnation,
        };
        self.probe = None;
        self.broadcast(update);
        self.changed();
        info!("leaving cluster"; node = %self.local, incarnation = self.incarnation());
        if self.live_peer_count() == 0 {
            self.state = LocalState::Left;
            return out;
        }
        self.state = LocalState::Leaving;
        self.timers.gossip = now;
        self.tick_gossip(now, &mut out);
        out
    }

    /// Publishes new metadata for the local node: bumps the incarnation so
    /// the new `Alive` record supersedes the old one everywhere.
    pub fn update_meta(&mut self, meta: Meta) -> Result<()> {
        validate_meta(&meta)?;
        if matches!(self.state, LocalState::Leaving | LocalState::Left) {
            return Err(Error::Stopped);
        }
        let me = self.local_member_mut();
        if me.meta == meta {
            return Ok(());
        }
        me.meta = meta;
        me.incarnation = me.incarnation.saturating_add(1);
        let snapshot = me.clone();
        info!("publishing new metadata"; node = %snapshot.id, incarnation = snapshot.incarnation);
        self.broadcast(Update::alive(&snapshot));
        self.emit(MembershipEvent::Updated(snapshot));
        self.changed();
        Ok(())
    }

    fn next_seq(&mut self) -> u64 {
        self.seq = self.seq.wrapping_add(1);
        self.seq
    }

    fn changed(&mut self) {
        self.version += 1;
    }

    fn emit(&mut self, event: MembershipEvent) {
        self.events.push(event);
    }

    /// Queues an update for gossip.
    fn broadcast(&mut self, update: Update) {
        let node = update.node().clone();
        self.gossip.push(node, update.to_bytes());
    }

    /// Sends `message` to `to`, filling the rest of the datagram with
    /// queued updates.
    fn send(&mut self, out: &mut Vec<Outbound>, to: SocketAddr, message: Message) {
        let frame = FrameWriter::new(self.cluster, &self.local, &message);
        let budget = frame.update_budget(self.config.max_packet_size);
        let limit = self.config.retransmit_limit(self.live_count());
        let updates = self.gossip.select(budget, limit);
        let bytes = frame.finish(updates.iter().map(|u| &u[..]));
        self.stats.packets_sent += 1;
        out.push(Outbound { to, bytes });
    }

    /// Sends one specific update to `to` right away, outside the gossip
    /// queue, e.g. to tell a member it is suspected.
    fn send_update(&mut self, out: &mut Vec<Outbound>, to: SocketAddr, update: &Update) {
        let frame = FrameWriter::new(self.cluster, &self.local, &Message::Gossip);
        let encoded = update.to_bytes();
        let bytes = frame.finish(std::iter::once(encoded.as_slice()));
        self.stats.packets_sent += 1;
        out.push(Outbound { to, bytes });
    }

    /// A gossip round: when updates are pending, send them to a few random
    /// members, including recently dead ones so that a member wrongly
    /// declared dead learns about it and refutes.
    fn tick_gossip(&mut self, now: Timestamp, out: &mut Vec<Outbound>) {
        if now < self.timers.gossip {
            return;
        }
        self.timers.gossip = now + self.config.gossip_interval;
        if self.gossip.is_empty() {
            return;
        }
        let to_dead = self.config.gossip_to_dead;
        let mut targets: Vec<SocketAddr> = self
            .members
            .values()
            .filter(|m| m.id != self.local)
            .filter(|m| m.is_live() || (m.state == MemberState::Dead && now < m.since + to_dead))
            .map(|m| m.gossip_addr)
            .collect();
        self.rng.shuffle(&mut targets);
        targets.truncate(self.config.gossip_fanout);
        for to in targets {
            if self.gossip.is_empty() {
                break;
            }
            self.send(out, to, Message::Gossip);
        }
    }

    fn tick_reap(&mut self, now: Timestamp) {
        if now < self.timers.reap {
            return;
        }
        self.timers.reap = now + REAP_CHECK;
        let ttl = self.config.tombstone_ttl;
        let expired: Vec<NodeId> = self
            .members
            .values()
            .filter(|m| !m.is_live() && m.id != self.local && now >= m.since + ttl)
            .map(|m| m.id.clone())
            .collect();
        for id in expired {
            if let Some(member) = self.members.remove(&id) {
                self.dead_notices.remove(&id);
                debug!("forgot member"; member = %id, state = %member.state);
                self.emit(MembershipEvent::Reaped(member));
                self.changed();
            }
        }
    }
}

/// A uniformly random delay in `[0, period)`.
fn stagger(rng: &mut Rng, period: Duration) -> Duration {
    let millis = u64::try_from(period.as_millis()).unwrap_or(u64::MAX).max(1);
    Duration::from_millis(rng.gen_range(0..millis))
}

/// The first deadline of a duty with period `period` (zero: disabled),
/// staggered within the first period.
fn enabled_after(now: Timestamp, period: Duration, rng: &mut Rng) -> Timestamp {
    if period.is_zero() {
        Timestamp::MAX
    } else {
        now + stagger(rng, period)
    }
}
