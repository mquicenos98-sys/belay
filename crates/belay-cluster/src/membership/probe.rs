//! Failure detection: one probe per protocol period.
//!
//! Each period the node pings the next member of a shuffled list (SWIM's
//! randomized round-robin, which bounds the time between probes of any one
//! member, unlike picking uniformly at random). Without an ack after
//! `probe_timeout`, it asks `indirect_checks` other members to ping the
//! target on its behalf, which separates "the target is down" from "the
//! path between us is lossy". If no ack, direct or relayed, arrives by the
//! end of the period, the target becomes suspect.

use std::net::SocketAddr;

use belay_common::{debug, Timestamp};

use super::{Membership, Outbound};
use crate::member::{MemberState, NodeId};
use crate::wire::{Message, Update};

/// Upper bound on pings relayed for others at once, so a flood of ping
/// requests cannot grow memory without limit.
const MAX_RELAYS: usize = 1024;

/// The probe of the current protocol period.
#[derive(Clone, Debug)]
pub(super) struct Probe {
    target: NodeId,
    addr: SocketAddr,
    /// The target's incarnation when the probe started; a failed probe
    /// suspects exactly this incarnation, so a refutation that arrived in
    /// the meantime wins.
    incarnation: u64,
    seq: u64,
    indirect_at: Timestamp,
    ends_at: Timestamp,
    acked: bool,
    indirect_sent: bool,
}

impl Probe {
    pub(super) fn next_deadline(&self) -> Timestamp {
        if self.acked || self.indirect_sent {
            self.ends_at
        } else {
            self.indirect_at.min(self.ends_at)
        }
    }
}

/// A ping sent on behalf of another member's ping request.
#[derive(Clone, Copy, Debug)]
pub(super) struct Relay {
    /// Where to forward the ack.
    reply_to: SocketAddr,
    /// The requester's seq, echoed in the forwarded ack.
    seq: u64,
    pub(super) expires: Timestamp,
}

impl Membership {
    pub(super) fn tick_probe(&mut self, now: Timestamp, out: &mut Vec<Outbound>) {
        let mut indirect = None;
        let mut finished = false;
        if let Some(probe) = &mut self.probe {
            if !probe.acked && !probe.indirect_sent && now >= probe.indirect_at {
                probe.indirect_sent = true;
                indirect = Some((probe.seq, probe.target.clone(), probe.addr));
            }
            finished = now >= probe.ends_at;
        }
        if let Some((seq, target, addr)) = indirect {
            self.send_ping_reqs(seq, target, addr, out);
        }
        if finished {
            if let Some(probe) = self.probe.take() {
                if !probe.acked {
                    self.probe_failed(probe, now, out);
                }
            }
        }
        if self.probe.is_none() && now >= self.timers.probe {
            self.timers.probe = now + self.config.probe_interval;
            self.start_probe(now, out);
        }
    }

    fn start_probe(&mut self, now: Timestamp, out: &mut Vec<Outbound>) {
        let Some(target) = self.next_probe_target() else {
            return;
        };
        let Some(member) = self.members.get(&target) else {
            return;
        };
        let (addr, incarnation) = (member.gossip_addr, member.incarnation);
        let seq = self.next_seq();
        self.probe = Some(Probe {
            target: target.clone(),
            addr,
            incarnation,
            seq,
            indirect_at: now + self.config.probe_timeout,
            ends_at: now + self.config.probe_interval,
            acked: false,
            indirect_sent: false,
        });
        self.stats.probes += 1;
        self.send(out, addr, Message::Ping { seq, target });
    }

    /// The next live member in the round-robin order, reshuffling at the
    /// end of each round. Members that died or left since the shuffle are
    /// skipped.
    fn next_probe_target(&mut self) -> Option<NodeId> {
        for _ in 0..2 {
            while self.probe_index < self.probe_list.len() {
                let id = &self.probe_list[self.probe_index];
                self.probe_index += 1;
                if *id != self.local && self.members.get(id).is_some_and(|m| m.is_live()) {
                    return Some(id.clone());
                }
            }
            self.probe_list = self
                .members
                .values()
                .filter(|m| m.id != self.local && m.is_live())
                .map(|m| m.id.clone())
                .collect();
            self.rng.shuffle(&mut self.probe_list);
            self.probe_index = 0;
            if self.probe_list.is_empty() {
                return None;
            }
        }
        None
    }

    /// Puts a new (or returning) member at a random position among the
    /// members not yet probed this round, so it is probed soon without
    /// making its position predictable.
    pub(super) fn add_to_probe_list(&mut self, id: &NodeId) {
        let start = self.probe_index.min(self.probe_list.len());
        if self.probe_list[start..].contains(id) {
            return;
        }
        let at = self.rng.gen_range(start..=self.probe_list.len());
        self.probe_list.insert(at, id.clone());
    }

    fn send_ping_reqs(
        &mut self,
        seq: u64,
        target: NodeId,
        target_addr: SocketAddr,
        out: &mut Vec<Outbound>,
    ) {
        // Only members not themselves under suspicion are asked for help.
        let mut helpers: Vec<SocketAddr> = self
            .members
            .values()
            .filter(|m| m.state == MemberState::Alive && m.id != self.local && m.id != target)
            .map(|m| m.gossip_addr)
            .collect();
        self.rng.shuffle(&mut helpers);
        helpers.truncate(self.config.indirect_checks);
        if helpers.is_empty() {
            return;
        }
        self.stats.indirect_probes += 1;
        debug!(
            "no ack, probing indirectly";
            target = %target,
            helpers = helpers.len()
        );
        for helper in helpers {
            self.send(
                out,
                helper,
                Message::PingReq {
                    seq,
                    target: target.clone(),
                    target_addr,
                },
            );
        }
    }

    fn probe_failed(&mut self, probe: Probe, now: Timestamp, out: &mut Vec<Outbound>) {
        self.stats.probe_failures += 1;
        debug!(
            "probe failed";
            target = %probe.target,
            indirect = probe.indirect_sent
        );
        let local = self.local.clone();
        if self.apply_suspect(probe.target.clone(), probe.incarnation, local.clone(), now) {
            // Tell the suspect directly: it can refute right away instead of
            // waiting for the rumour to reach it.
            let update = Update::Suspect {
                id: probe.target,
                incarnation: probe.incarnation,
                from: local,
            };
            self.send_update(out, probe.addr, &update);
        }
    }

    pub(super) fn on_ping(
        &mut self,
        from: SocketAddr,
        seq: u64,
        target: &NodeId,
        out: &mut Vec<Outbound>,
    ) {
        if *target != self.local {
            // The prober knows this address under another id: we took over
            // the address of a node that has gone. Staying silent lets the
            // prober find out through the normal failure detection.
            debug!("ignoring ping for another node"; target = %target, from = %from);
            return;
        }
        self.send(out, from, Message::Ack { seq });
    }

    pub(super) fn on_ack(&mut self, seq: u64, out: &mut Vec<Outbound>) {
        if let Some(probe) = &mut self.probe {
            if probe.seq == seq {
                probe.acked = true;
                return;
            }
        }
        if let Some(relay) = self.relays.remove(&seq) {
            self.send(out, relay.reply_to, Message::Ack { seq: relay.seq });
        }
    }

    pub(super) fn on_ping_req(
        &mut self,
        from: SocketAddr,
        seq: u64,
        target: NodeId,
        target_addr: SocketAddr,
        now: Timestamp,
        out: &mut Vec<Outbound>,
    ) {
        if target == self.local {
            // Asked to probe ourselves: the requester can reach us after
            // all, so answer it directly.
            self.send(out, from, Message::Ack { seq });
            return;
        }
        if self.relays.len() >= MAX_RELAYS {
            debug!("too many relayed probes, dropping ping request"; from = %from);
            return;
        }
        let relay_seq = self.next_seq();
        self.relays.insert(
            relay_seq,
            Relay {
                reply_to: from,
                seq,
                expires: now + self.config.probe_interval,
            },
        );
        self.send(
            out,
            target_addr,
            Message::Ping {
                seq: relay_seq,
                target,
            },
        );
    }

    pub(super) fn expire_relays(&mut self, now: Timestamp) {
        self.relays.retain(|_, relay| relay.expires > now);
    }
}
