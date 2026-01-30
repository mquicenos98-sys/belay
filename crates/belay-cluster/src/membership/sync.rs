//! Full-state exchanges: joining, anti-entropy and reconnection.
//!
//! Gossip is fast but forgetful (each update is sent a bounded number of
//! times), so a node that missed an update, or was cut off while it
//! spread, would never learn it. Three duties exchange whole views to
//! close those gaps, all with the same `Sync` message:
//!
//! - **Join**: a new node sends its state to every seed until one answers
//!   with its own view.
//! - **Anti-entropy**: every `push_pull_interval` (scaled for large
//!   clusters) a node swaps views with one random live member.
//! - **Reconnect**: every `reconnect_interval` a node sends its view to one
//!   random member it believes dead, and to its seeds while it has no live
//!   peers. After a partition heals, this is what lets the two sides find
//!   each other again: each receiver sees itself listed as dead, refutes,
//!   and the refutations spread through the reconnected cluster.
//!
//! Remote `Dead` records are merged as suspicions: a peer's stale belief
//! that a member died must not kill it here without the member getting a
//! chance to refute through the local suspicion timer.

use std::net::SocketAddr;

use belay_common::{debug, info, Timestamp};

use super::merge::AliveInfo;
use super::{LocalState, Membership, Outbound};
use crate::member::{MemberState, NodeId};
use crate::wire::{self, MemberRecord};

impl Membership {
    /// Sends the full local view to `to`, split across as many datagrams
    /// as needed.
    pub(super) fn send_sync(&mut self, out: &mut Vec<Outbound>, to: SocketAddr, reply: bool) {
        let records: Vec<MemberRecord> = self.members.values().map(MemberRecord::from).collect();
        let datagrams = wire::encode_sync(
            self.cluster,
            &self.local,
            reply,
            &records,
            self.config.max_packet_size,
        );
        self.stats.packets_sent += datagrams.len() as u64;
        if reply {
            self.stats.syncs_sent += 1;
        }
        out.extend(datagrams.into_iter().map(|bytes| Outbound { to, bytes }));
    }

    pub(super) fn tick_join(&mut self, now: Timestamp, out: &mut Vec<Outbound>) {
        if now < self.timers.join || self.seeds.is_empty() {
            return;
        }
        let reconnect = self.config.reconnect_interval;
        let retry = match self.state {
            LocalState::Joining => self.config.join_retry_interval,
            // Everyone we knew is gone: keep knocking on the seeds, but at
            // the slower reconnect pace.
            LocalState::Active if !reconnect.is_zero() && self.live_peer_count() == 0 => {
                reconnect.max(self.config.join_retry_interval)
            }
            _ => {
                self.timers.join = now + self.config.join_retry_interval;
                return;
            }
        };
        self.timers.join = now + retry;
        debug!(
            "contacting seeds";
            node = %self.local,
            seeds = self.seeds.len(),
            state = %self.state
        );
        for seed in self.seeds.clone() {
            self.send_sync(out, seed, true);
        }
    }

    pub(super) fn tick_push_pull(&mut self, now: Timestamp, out: &mut Vec<Outbound>) {
        if now < self.timers.push_pull {
            return;
        }
        let n = self.live_count();
        self.timers.push_pull = now + self.config.push_pull_period(n);
        let peers: Vec<SocketAddr> = self
            .members
            .values()
            .filter(|m| m.state == MemberState::Alive && m.id != self.local)
            .map(|m| m.gossip_addr)
            .collect();
        if let Some(&peer) = self.rng.choose(&peers) {
            self.send_sync(out, peer, true);
        }
    }

    pub(super) fn tick_reconnect(&mut self, now: Timestamp, out: &mut Vec<Outbound>) {
        if now < self.timers.reconnect {
            return;
        }
        self.timers.reconnect = now + self.config.reconnect_interval;
        let dead: Vec<(NodeId, SocketAddr)> = self
            .members
            .values()
            .filter(|m| m.state == MemberState::Dead)
            .map(|m| (m.id.clone(), m.gossip_addr))
            .collect();
        if let Some((id, addr)) = self.rng.choose(&dead).cloned() {
            debug!("attempting to reach dead member"; member = %id, addr = %addr);
            self.send_sync(out, addr, true);
        }
    }

    pub(super) fn on_sync(
        &mut self,
        from: SocketAddr,
        sender: &NodeId,
        reply: bool,
        records: Vec<MemberRecord>,
        now: Timestamp,
        out: &mut Vec<Outbound>,
    ) {
        self.stats.syncs_received += 1;
        for record in records {
            self.merge_record(record, sender, now);
        }
        if self.state == LocalState::Joining {
            self.state = LocalState::Active;
            info!(
                "joined cluster";
                node = %self.local,
                via = %sender,
                members = self.live_count()
            );
        }
        if reply {
            self.send_sync(out, from, false);
        }
    }

    fn merge_record(&mut self, record: MemberRecord, sender: &NodeId, now: Timestamp) {
        let alive = |r: MemberRecord| AliveInfo {
            id: r.id,
            incarnation: r.incarnation,
            addr: r.addr,
            gossip_addr: r.gossip_addr,
            meta: r.meta,
        };
        match record.state {
            MemberState::Alive => self.apply_alive(alive(record), now),
            MemberState::Left => self.apply_left(record.id, record.incarnation, now),
            MemberState::Suspect | MemberState::Dead => {
                let (id, incarnation) = (record.id.clone(), record.incarnation);
                if id != self.local && !self.members.contains_key(&id) {
                    // Learn about a suspect we have never heard of (it may
                    // well refute), but not about the long dead.
                    if record.state == MemberState::Dead {
                        return;
                    }
                    self.apply_alive(alive(record), now);
                }
                self.apply_suspect(id, incarnation, sender.clone(), now);
            }
        }
    }
}
