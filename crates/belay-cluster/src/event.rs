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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::member::{MemberState, Meta, NodeId};
    use belay_common::Timestamp;

    fn member(state: MemberState) -> Member {
        Member {
            id: NodeId::new("n2").unwrap(),
            addr: String::new(),
            gossip_addr: "10.0.0.2:7946".parse().unwrap(),
            state,
            incarnation: 1,
            meta: Meta::new(),
            since: Timestamp::EPOCH,
        }
    }

    #[test]
    fn exposes_member_name_and_ownership_impact() {
        let cases = [
            (
                MembershipEvent::Joined(member(MemberState::Alive)),
                "joined",
                true,
            ),
            (
                MembershipEvent::Updated(member(MemberState::Alive)),
                "updated",
                true,
            ),
            (
                MembershipEvent::Suspected(member(MemberState::Suspect)),
                "suspected",
                false,
            ),
            (
                MembershipEvent::Recovered(member(MemberState::Alive)),
                "recovered",
                false,
            ),
            (
                MembershipEvent::Failed(member(MemberState::Dead)),
                "failed",
                true,
            ),
            (
                MembershipEvent::Left(member(MemberState::Left)),
                "left",
                true,
            ),
            (
                MembershipEvent::Reaped(member(MemberState::Dead)),
                "reaped",
                false,
            ),
        ];
        for (event, name, affects) in cases {
            assert_eq!(event.name(), name);
            assert_eq!(event.affects_ownership(), affects, "{name}");
            assert_eq!(event.member().id, "n2");
            assert_eq!(event.clone().into_member(), *event.member());
        }
    }

    #[test]
    fn display_names_the_event_and_member() {
        let e = MembershipEvent::Failed(member(MemberState::Dead));
        assert_eq!(
            e.to_string(),
            "member failed: n2 (10.0.0.2:7946, dead, incarnation 1)"
        );
    }
}
