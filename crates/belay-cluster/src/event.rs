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

impl MembershipEvent {
    /// The member the event is about.
    #[must_use]
    pub fn member(&self) -> &Member {
        match self {
            MembershipEvent::Joined(m)
            | MembershipEvent::Updated(m)
            | MembershipEvent::Suspected(m)
            | MembershipEvent::Recovered(m)
            | MembershipEvent::Failed(m)
            | MembershipEvent::Left(m)
            | MembershipEvent::Reaped(m) => m,
        }
    }

    /// Consumes the event, returning the member.
    #[must_use]
    pub fn into_member(self) -> Member {
        match self {
            MembershipEvent::Joined(m)
            | MembershipEvent::Updated(m)
            | MembershipEvent::Suspected(m)
            | MembershipEvent::Recovered(m)
            | MembershipEvent::Failed(m)
            | MembershipEvent::Left(m)
            | MembershipEvent::Reaped(m) => m,
        }
    }

    /// Lowercase name of the event kind.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            MembershipEvent::Joined(_) => "joined",
            MembershipEvent::Updated(_) => "updated",
            MembershipEvent::Suspected(_) => "suspected",
            MembershipEvent::Recovered(_) => "recovered",
            MembershipEvent::Failed(_) => "failed",
            MembershipEvent::Left(_) => "left",
            MembershipEvent::Reaped(_) => "reaped",
        }
    }

    /// Whether ownership may have changed: the live set changed, or a live
    /// member's metadata (and so possibly its weight) did. Suspicion and
    /// recovery leave ownership alone, since suspects keep their keys.
    #[must_use]
    pub const fn affects_ownership(&self) -> bool {
        matches!(
            self,
            MembershipEvent::Joined(_)
                | MembershipEvent::Updated(_)
                | MembershipEvent::Failed(_)
                | MembershipEvent::Left(_)
        )
    }
}

impl fmt::Display for MembershipEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "member {}: {}", self.name(), self.member())
    }
}
