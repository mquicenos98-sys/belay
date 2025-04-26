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
        self.cursor.ring = cursor.ring;
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
