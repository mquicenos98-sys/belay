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
