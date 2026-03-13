//! Fair sharing of a queue between tenants: deficit round robin over
//! per-tenant ready sets.
//!
//! Every tenant with ready jobs has a *flow*, an ordered set of its ready
//! jobs (priority, then eligibility time, then id). Flows sit in a ring.
//! The tenant at the front gets a turn of `quantum` picks (its weight
//! times the base quantum); when the turn is used up it moves to the back,
//! and when its flow empties it leaves the ring. A turn interrupted by the
//! end of a lease request carries over to the next request: the unused
//! credit is the "deficit" that makes the share exact over time rather
//! than per request.
//!
//! Jobs without a tenant form one flow of their own, so a queue that never
//! uses tenants behaves as a plain priority queue.
//!
//! Priority orders jobs *within* a tenant; it does not let one tenant's
//! urgent jobs overtake another tenant's turn. That is the point of fair
//! sharing: a tenant cannot buy more than its share by raising priorities.
//!
//! Lease requests plan several picks before anything is committed, so
//! picking works on a [`Planner`]: a copy of the ring position plus a count
//! of jobs taken per flow, leaving the committed state untouched until the
//! store write succeeds and the plan's cursor is installed.

use std::collections::{BTreeSet, HashMap, VecDeque};

use belay_proto::TenantId;

use crate::meta::ReadyKey;

/// A tenant, or `None` for jobs that name none.
pub(crate) type TenantKey = Option<TenantId>;

/// Position of the round robin: the ring of tenants with ready jobs and the
/// picks left in the front tenant's current turn (0 = turn not started).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FairCursor {
    ring: VecDeque<TenantKey>,
    credit: u32,
}

/// The ready jobs of one queue, grouped into per-tenant flows.
#[derive(Debug, Default)]
pub(crate) struct FairQueue {
    flows: HashMap<TenantKey, BTreeSet<ReadyKey>>,
    cursor: FairCursor,
    len: usize,
}

impl FairQueue {
    /// Adds a ready job. A tenant whose flow was empty joins the back of
    /// the ring.
    pub(crate) fn insert(&mut self, tenant: &TenantKey, key: ReadyKey) {
        let flow = match self.flows.get_mut(tenant) {
            Some(flow) => flow,
            None => {
                self.cursor.ring.push_back(tenant.clone());
                self.flows.entry(tenant.clone()).or_default()
            }
        };
        if flow.insert(key) {
            self.len += 1;
        }
    }

    /// Removes a ready job; returns whether it was present. A tenant whose
    /// flow empties leaves the ring, forfeiting the rest of its turn.
    pub(crate) fn remove(&mut self, tenant: &TenantKey, key: &ReadyKey) -> bool {
        let Some(flow) = self.flows.get_mut(tenant) else {
            return false;
        };
        if !flow.remove(key) {
            return false;
        }
        self.len -= 1;
        if flow.is_empty() {
            self.flows.remove(tenant);
            if let Some(pos) = self.cursor.ring.iter().position(|t| t == tenant) {
                self.cursor.ring.remove(pos);
                if pos == 0 {
                    self.cursor.credit = 0;
                }
            }
        }
        true
    }

    /// Ready jobs in all flows.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Tenants with ready jobs.
    pub(crate) fn tenants(&self) -> usize {
        self.flows.len()
    }

    /// Every ready job, flow by flow (no particular flow order).
    pub(crate) fn iter(&self) -> impl Iterator<Item = &ReadyKey> + '_ {
        self.flows.values().flat_map(|flow| flow.iter())
    }

    /// Starts planning picks from the current position.
    pub(crate) fn planner(&self) -> Planner<'_> {
        Planner {
            queue: self,
            cursor: self.cursor.clone(),
            taken: HashMap::new(),
        }
    }

    /// Adopts the position a committed plan ended at.
    ///
    /// Only valid when the plan's picks are removed right after (as a
    /// lease commit does): the plan already dropped tenants whose flows it
    /// exhausted, and the removals then find nothing more to drop.
    pub(crate) fn install(&mut self, cursor: FairCursor) {
        self.cursor = cursor;
    }

    #[cfg(test)]
    fn ring(&self) -> Vec<TenantKey> {
        self.cursor.ring.iter().cloned().collect()
    }
}

/// Picks jobs in fair order without changing the queue.
#[derive(Debug)]
pub(crate) struct Planner<'a> {
    queue: &'a FairQueue,
    cursor: FairCursor,
    taken: HashMap<TenantKey, usize>,
}

impl Planner<'_> {
    /// Whether another pick is possible.
    pub(crate) fn has_next(&self) -> bool {
        !self.cursor.ring.is_empty()
    }

    /// The next job in fair order. `quantum` gives each tenant's picks per
    /// turn (at least 1).
    pub(crate) fn next(&mut self, quantum: impl Fn(&TenantKey) -> u32) -> Option<ReadyKey> {
        loop {
            let tenant = self.cursor.ring.front()?.clone();
            let taken = self.taken.get(&tenant).copied().unwrap_or(0);
            let flow = self.queue.flows.get(&tenant);
            // Planned picks come off the front of a flow in order, so the
            // next one is the first not yet taken. Plans are at most a lease
            // batch long, which keeps the skip cheap.
            let Some(key) = flow.and_then(|f| f.iter().nth(taken)).copied() else {
                // Exhausted by this plan; the pick that exhausted it normally
                // drops it already, this covers a ring that got ahead of its
                // flows.
                self.cursor.ring.pop_front();
                self.cursor.credit = 0;
                continue;
            };
            if self.cursor.credit == 0 {
                self.cursor.credit = quantum(&tenant).max(1);
            }
            self.cursor.credit -= 1;
            self.taken.insert(tenant, taken + 1);
            let remaining = flow.map_or(0, BTreeSet::len) - (taken + 1);
            if remaining == 0 {
                self.cursor.ring.pop_front();
                self.cursor.credit = 0;
            } else if self.cursor.credit == 0 {
                self.cursor.ring.rotate_left(1);
            }
            return Some(key);
        }
    }

    /// The position after the planned picks, for [`FairQueue::install`].
    pub(crate) fn into_cursor(self) -> FairCursor {
        self.cursor
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_common::{Timestamp, Ulid};
    use belay_proto::JobId;

    fn tenant(name: &str) -> TenantKey {
        Some(name.parse().expect("tenant"))
    }

    fn key(n: u128, priority: i16) -> ReadyKey {
        ReadyKey::new(
            priority,
            Timestamp::from_secs(0),
            JobId::from_ulid(Ulid::from_u128(n)),
        )
    }

    fn n(k: ReadyKey) -> u128 {
        k.id.ulid().to_u128()
    }

    /// Plans `count` picks and commits them like a lease would.
    fn take(q: &mut FairQueue, count: usize, quantum: u32) -> Vec<u128> {
        let mut planner = q.planner();
        let mut picks = Vec::new();
        while picks.len() < count {
            match planner.next(|_| quantum) {
                Some(k) => picks.push(k),
                None => break,
            }
        }
        let cursor = planner.into_cursor();
        q.install(cursor);
        for k in &picks {
            let owner = q
                .flows
                .iter()
                .find(|(_, f)| f.contains(k))
                .map(|(t, _)| t.clone())
                .expect("picked job belongs to a flow");
            assert!(q.remove(&owner, k));
        }
        picks.into_iter().map(n).collect()
    }

    #[test]
    fn a_single_flow_is_a_priority_queue() {
        let mut q = FairQueue::default();
        q.insert(&None, key(1, 0));
        q.insert(&None, key(2, 5));
        q.insert(&None, key(3, -5));
        q.insert(&None, key(4, 5));
        assert_eq!(take(&mut q, 10, 1), [2, 4, 1, 3]);
        assert!(q.is_empty());
        assert_eq!(q.tenants(), 0);
    }

    #[test]
    fn tenants_alternate_regardless_of_arrival_order() {
        let mut q = FairQueue::default();
        for i in 0..6 {
            q.insert(&tenant("big"), key(i, 0));
        }
        q.insert(&tenant("small"), key(100, 0));
        q.insert(&tenant("small"), key(101, 0));
        assert_eq!(take(&mut q, 4, 1), [0, 100, 1, 101]);
        assert_eq!(q.ring(), [tenant("big")]);
        assert_eq!(take(&mut q, 10, 1), [2, 3, 4, 5]);
    }

    #[test]
    fn priority_does_not_jump_the_tenant_queue() {
        let mut q = FairQueue::default();
        q.insert(&tenant("a"), key(1, 0));
        q.insert(&tenant("a"), key(2, 0));
        q.insert(&tenant("b"), key(3, 1000));
        q.insert(&tenant("b"), key(4, 1000));
        assert_eq!(take(&mut q, 4, 1), [1, 3, 2, 4]);
    }

    #[test]
    fn quantum_gives_bursts_and_turns_carry_over_between_requests() {
        let mut q = FairQueue::default();
        for i in 0..6 {
            q.insert(&tenant("a"), key(i, 0));
            q.insert(&tenant("b"), key(10 + i, 0));
        }
        // A turn of three is split across two single-job requests...
        assert_eq!(take(&mut q, 1, 3), [0]);
        assert_eq!(take(&mut q, 1, 3), [1]);
        // ...and finishes before b gets its turn.
        assert_eq!(take(&mut q, 4, 3), [2, 10, 11, 12]);
        assert_eq!(take(&mut q, 20, 3), [3, 4, 5, 13, 14, 15]);
    }

    #[test]
    fn weights_scale_the_share() {
        let mut q = FairQueue::default();
        for i in 0..8 {
            q.insert(&tenant("gold"), key(i, 0));
            q.insert(&tenant("free"), key(100 + i, 0));
        }
        let mut planner = q.planner();
        let quantum = |t: &TenantKey| if *t == tenant("gold") { 3 } else { 1 };
        let picks: Vec<u128> = (0..8)
            .filter_map(|_| planner.next(quantum))
            .map(n)
            .collect();
        assert_eq!(picks, [0, 1, 2, 100, 3, 4, 5, 101]);
    }

    #[test]
    fn planning_leaves_the_queue_untouched() {
        let mut q = FairQueue::default();
        q.insert(&tenant("a"), key(1, 0));
        q.insert(&tenant("b"), key(2, 0));
        let before = q.cursor.clone();
        let mut planner = q.planner();
        assert_eq!(planner.next(|_| 1).map(n), Some(1));
        assert_eq!(planner.next(|_| 1).map(n), Some(2));
        assert!(!planner.has_next());
        assert_eq!(planner.next(|_| 1), None);
        drop(planner);
        assert_eq!(q.cursor, before);
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn removing_the_front_tenant_resets_its_turn() {
        let mut q = FairQueue::default();
        q.insert(&tenant("a"), key(1, 0));
        q.insert(&tenant("a"), key(2, 0));
        q.insert(&tenant("b"), key(3, 0));
        assert_eq!(take(&mut q, 1, 2), [1]);
        // a still has one pick left in its turn, but its last job is
        // cancelled: b goes next with a fresh turn.
        assert!(q.remove(&tenant("a"), &key(2, 0)));
        assert!(!q.remove(&tenant("a"), &key(2, 0)), "already gone");
        assert_eq!(q.ring(), [tenant("b")]);
        assert_eq!(q.cursor.credit, 0);
        q.insert(&tenant("a"), key(4, 0));
        assert_eq!(take(&mut q, 5, 2), [3, 4]);
    }

    #[test]
    fn untenanted_jobs_compete_as_one_tenant() {
        let mut q = FairQueue::default();
        q.insert(&None, key(1, 0));
        q.insert(&None, key(2, 0));
        q.insert(&tenant("t"), key(3, 0));
        assert_eq!(q.tenants(), 2);
        assert_eq!(q.iter().count(), 3);
        assert_eq!(take(&mut q, 3, 1), [1, 3, 2]);
    }
}
