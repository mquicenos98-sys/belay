//! Applying membership updates: SWIM precedence, suspicion timers and
//! refutation.
//!
//! Precedence, for updates about a member currently at incarnation `i`:
//!
//! | update | accepted when |
//! |---|---|
//! | `Alive(j)` | `j > i` (brings dead and departed members back too) |
//! | `Suspect(j)` | member is live and `j >= i`; at `j == i` a suspect only gains a confirmation |
//! | `Dead(j)` | member is live and `j >= i` |
//! | `Left(j)` | member has not already left and `j >= i` |
//!
//! Every accepted update is queued for gossip, so news spreads from each
//! node that learns it. Updates about the local node are never applied:
//! any claim that it is suspect, dead or gone, or an `Alive` record newer
//! than its own (left over from a previous run), is answered by bumping the
//! incarnation past the claim and gossiping a fresh `Alive`.

use std::net::SocketAddr;

use belay_common::{debug, info, Timestamp};

use super::{LocalState, Membership, Outbound};
use crate::event::MembershipEvent;
use crate::member::{Member, MemberState, Meta, NodeId};
use crate::suspicion::Suspicion;
use crate::wire::Update;

/// The contents of an `Alive` update or record.
pub(super) struct AliveInfo {
    pub(super) id: NodeId,
    pub(super) incarnation: u64,
    pub(super) addr: String,
    pub(super) gossip_addr: SocketAddr,
    pub(super) meta: Meta,
}

impl AliveInfo {
    fn same_data(&self, m: &Member) -> bool {
        m.addr == self.addr && m.gossip_addr == self.gossip_addr && m.meta == self.meta
    }
}

impl Membership {
    pub(super) fn apply_update(&mut self, update: Update, now: Timestamp) {
        match update {
            Update::Alive {
                id,
                incarnation,
                addr,
                gossip_addr,
                meta,
            } => self.apply_alive(
                AliveInfo {
                    id,
                    incarnation,
                    addr,
                    gossip_addr,
                    meta,
                },
                now,
            ),
            Update::Suspect {
                id,
                incarnation,
                from,
            } => {
                self.apply_suspect(id, incarnation, from, now);
            }
            Update::Dead {
                id,
                incarnation,
                from,
            } => self.apply_dead(id, incarnation, &from, now),
            Update::Left { id, incarnation } => self.apply_left(id, incarnation, now),
        }
    }

    pub(super) fn apply_alive(&mut self, alive: AliveInfo, now: Timestamp) {
        if alive.id == self.local {
            let me = self.local_member();
            let newer = alive.incarnation > me.incarnation;
            let conflicting = alive.incarnation == me.incarnation && !alive.same_data(me);
            if (newer || conflicting) && self.state != LocalState::Leaving {
                self.refute(alive.incarnation);
            }
            return;
        }
        let Some(member) = self.members.get_mut(&alive.id) else {
            let member = Member {
                id: alive.id.clone(),
                addr: alive.addr,
                gossip_addr: alive.gossip_addr,
                state: MemberState::Alive,
                incarnation: alive.incarnation,
                meta: alive.meta,
                since: now,
            };
            info!(
                "member joined";
                member = %member.id,
                addr = %member.gossip_addr,
                incarnation = member.incarnation
            );
            self.add_to_probe_list(&member.id);
            self.broadcast(Update::alive(&member));
            self.members.insert(alive.id, member.clone());
            self.emit(MembershipEvent::Joined(member));
            self.changed();
            return;
        };
        if alive.incarnation <= member.incarnation {
            return;
        }
        let previous = member.state;
        let data_changed = !alive.same_data(member);
        let moved = member.gossip_addr != alive.gossip_addr;
        member.incarnation = alive.incarnation;
        member.addr = alive.addr;
        member.gossip_addr = alive.gossip_addr;
        member.meta = alive.meta;
        member.state = MemberState::Alive;
        if previous != MemberState::Alive {
            member.since = now;
        }
        let snapshot = member.clone();
        self.suspicions.remove(&snapshot.id);
        self.broadcast(Update::alive(&snapshot));
        match previous {
            MemberState::Alive => {
                if data_changed {
                    info!(
                        "member updated";
                        member = %snapshot.id,
                        incarnation = snapshot.incarnation,
                        moved
                    );
                    self.emit(MembershipEvent::Updated(snapshot));
                }
            }
            MemberState::Suspect => {
                debug!(
                    "suspicion refuted";
                    member = %snapshot.id,
                    incarnation = snapshot.incarnation
                );
                self.emit(MembershipEvent::Recovered(snapshot));
            }
            MemberState::Dead | MemberState::Left => {
                info!(
                    "member rejoined";
                    member = %snapshot.id,
                    addr = %snapshot.gossip_addr,
                    incarnation = snapshot.incarnation,
                    was = %previous
                );
                self.add_to_probe_list(&snapshot.id);
                self.emit(MembershipEvent::Joined(snapshot));
            }
        }
        self.changed();
    }

    /// Applies a suspicion of `id` at `incarnation` raised by `from`.
    /// Returns whether it started a new suspicion (as opposed to confirming
    /// one, or being stale).
    pub(super) fn apply_suspect(
        &mut self,
        id: NodeId,
        incarnation: u64,
        from: NodeId,
        now: Timestamp,
    ) -> bool {
        if id == self.local {
            if self.state != LocalState::Leaving && incarnation >= self.incarnation() {
                self.refute(incarnation);
            }
            return false;
        }
        let Some(member) = self.members.get(&id) else {
            return false;
        };
        if incarnation < member.incarnation || !member.is_live() {
            return false;
        }
        if member.state == MemberState::Suspect && incarnation == member.incarnation {
            let confirmed = self
                .suspicions
                .get_mut(&id)
                .is_some_and(|s| s.confirm(&from));
            if confirmed {
                // Pass the confirmation on so other members shorten their
                // timers too.
                self.broadcast(Update::Suspect {
                    id,
                    incarnation,
                    from,
                });
            }
            return false;
        }
        let n = self.live_count();
        // Confirmations can only come from members other than the suspect
        // and the first suspecter.
        let k = self
            .config
            .suspicion_confirmations()
            .min(u32::try_from(n.saturating_sub(2)).unwrap_or(u32::MAX));
        let suspicion = Suspicion::new(
            incarnation,
            from.clone(),
            now,
            self.config.suspicion_timeout(n),
            self.config.suspicion_max_timeout(n),
            k,
        );
        let member = self
            .members
            .get_mut(&id)
            .expect("looked up above without intervening removal");
        let was = member.state;
        member.state = MemberState::Suspect;
        member.incarnation = incarnation;
        if was != MemberState::Suspect {
            member.since = now;
        }
        let snapshot = member.clone();
        debug!(
            "member suspected";
            member = %id,
            incarnation,
            from = %from,
            timeout = suspicion.timeout()
        );
        self.suspicions.insert(id.clone(), suspicion);
        self.stats.suspicions += 1;
        self.broadcast(Update::Suspect {
            id,
            incarnation,
            from,
        });
        self.emit(MembershipEvent::Suspected(snapshot));
        self.changed();
        true
    }

    pub(super) fn apply_dead(
        &mut self,
        id: NodeId,
        incarnation: u64,
        from: &NodeId,
        now: Timestamp,
    ) {
        if id == self.local {
            if self.state != LocalState::Leaving && incarnation >= self.incarnation() {
                self.refute(incarnation);
            }
            return;
        }
        let Some(member) = self.members.get_mut(&id) else {
            return;
        };
        if incarnation < member.incarnation || !member.is_live() {
            return;
        }
        member.state = MemberState::Dead;
        member.incarnation = incarnation;
        member.since = now;
        let snapshot = member.clone();
        self.suspicions.remove(&id);
        info!(
            "member failed";
            member = %id,
            incarnation,
            declared_by = %from
        );
        self.broadcast(Update::Dead {
            id,
            incarnation,
            from: from.clone(),
        });
        self.emit(MembershipEvent::Failed(snapshot));
        self.changed();
    }

    pub(super) fn apply_left(&mut self, id: NodeId, incarnation: u64, now: Timestamp) {
        if id == self.local {
            // A record of our own departure from a previous run.
            if self.state != LocalState::Leaving && incarnation >= self.incarnation() {
                self.refute(incarnation);
            }
            return;
        }
        let Some(member) = self.members.get_mut(&id) else {
            return;
        };
        if incarnation < member.incarnation || member.state == MemberState::Left {
            return;
        }
        member.state = MemberState::Left;
        member.incarnation = incarnation;
        member.since = now;
        let snapshot = member.clone();
        self.suspicions.remove(&id);
        info!("member left"; member = %id, incarnation);
        self.broadcast(Update::Left { id, incarnation });
        self.emit(MembershipEvent::Left(snapshot));
        self.changed();
    }

    /// Moves the local incarnation past `claimed` and gossips `Alive`.
    fn refute(&mut self, claimed: u64) {
        let me = self.local_member_mut();
        me.incarnation = me.incarnation.max(claimed).saturating_add(1);
        let snapshot = me.clone();
        self.stats.refutations += 1;
        debug!(
            "refuting claim about the local node";
            claimed,
            incarnation = snapshot.incarnation
        );
        self.broadcast(Update::alive(&snapshot));
        self.changed();
    }

    /// Declares dead every suspect whose timer ran out without a refutation.
    pub(super) fn tick_suspicions(&mut self, now: Timestamp) {
        let expired: Vec<(NodeId, u64)> = self
            .suspicions
            .iter()
            .filter(|(_, s)| s.deadline() <= now)
            .map(|(id, s)| (id.clone(), s.incarnation()))
            .collect();
        let local = self.local.clone();
        for (id, incarnation) in expired {
            if let Some(s) = self.suspicions.remove(&id) {
                debug!(
                    "suspicion timed out";
                    member = %id,
                    confirmations = s.confirmations()
                );
            }
            self.apply_dead(id, incarnation, &local, now);
        }
    }

    /// Tells a member that this node believes dead, and that has just sent
    /// us something, what we believe: it will refute, which heals the view
    /// faster than waiting for gossip or a reconnect attempt. Rate-limited
    /// per member to one notice per protocol period.
    pub(super) fn notify_if_dead(
        &mut self,
        sender: &NodeId,
        from: SocketAddr,
        now: Timestamp,
        out: &mut Vec<Outbound>,
    ) {
        let Some(member) = self.members.get(sender) else {
            return;
        };
        if member.state != MemberState::Dead {
            return;
        }
        let recently = self
            .dead_notices
            .get(sender)
            .is_some_and(|at| now < *at + self.config.probe_interval);
        if recently {
            return;
        }
        let update = Update::Dead {
            id: sender.clone(),
            incarnation: member.incarnation,
            from: self.local.clone(),
        };
        self.dead_notices.insert(sender.clone(), now);
        self.send_update(out, from, &update);
    }
}
