//! Notifications of membership changes.

use std::fmt;

use crate::member::Member;

/// A change in the local node's view of another member.
///
/// Each event carries a snapshot of the member *after* the change. Events
/// are produced by [`Membership::drain_events`](crate::Membership::drain_events)
/// and delivered by [`Node::subscribe`](crate::Node::subscribe).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MembershipEvent {
    /// A member appeared, or came back after being dead or gone.
    Joined(Member),
    /// A live member published a new address or new metadata.
    Updated(Member),
    /// A member missed a probe and is suspected of having failed.
    Suspected(Member),
    /// A suspected member refuted the suspicion.
    Recovered(Member),
    /// A member was declared dead.
    Failed(Member),
    /// A member left gracefully.
    Left(Member),
    /// A dead or departed member was forgotten after the tombstone TTL.
    Reaped(Member),
}
