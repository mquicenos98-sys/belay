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
