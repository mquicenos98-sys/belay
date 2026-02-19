//! Cluster membership as the engine uses it: which node owns a queue or a
//! schedule, and who the members are.
//!
//! Ownership is rendezvous hashing over the live members (see
//! belay-cluster), of the key `queue:<name>` for a queue and
//! `schedule:<id>` for a schedule. The engine itself serves whatever it is
//! asked; [`Engine::owner_of`](crate::Engine::owner_of) is for the HTTP
//! layer, which redirects requests for queues owned elsewhere. Schedules
//! fire only on their owner (other nodes move them past their fire times
//! without firing), and not at all until the node has joined through a
//! seed or its join grace period has passed.
//!
//! Without clustering every key is [`Ownership::Local`].

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use belay_cluster::{MemberState, Node, Transport, UdpTransport};
use belay_proto::api::{MemberInfo, MemberState as ApiMemberState};
use belay_proto::{QueueName, ScheduleId};

use crate::config::Config;
use crate::error::{Error, Result};

/// Where requests about a key should be served.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ownership {
    /// By this node.
    Local,
    /// By another node.
    Remote(RemoteOwner),
}

impl Ownership {
    /// Whether this node owns the key.
    #[must_use]
    pub fn is_local(&self) -> bool {
        matches!(self, Ownership::Local)
    }
}

/// The node owning a key that this one does not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteOwner {
    /// Its node id.
    pub node: String,
    /// Base URL of its HTTP API, e.g. `http://10.0.0.6:7700`.
    pub addr: String,
}

impl RemoteOwner {
    /// The absolute URL of `path_and_query` on the owner, for a redirect's
    /// `Location`.
    ///
    /// ```
    /// use belay_core::RemoteOwner;
    ///
    /// let owner = RemoteOwner { node: "b".into(), addr: "http://10.0.0.6:7700".into() };
    /// assert_eq!(owner.url("/v1/lease"), "http://10.0.0.6:7700/v1/lease");
    /// ```
    #[must_use]
    pub fn url(&self, path_and_query: &str) -> String {
        format!("{}{}", self.addr.trim_end_matches('/'), path_and_query)
    }
}

impl fmt::Display for RemoteOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.node, self.addr)
    }
}

/// The ownership key of a queue.
#[must_use]
pub fn queue_key(queue: &QueueName) -> String {
    format!("queue:{queue}")
}

/// The ownership key of a schedule.
#[must_use]
pub fn schedule_key(id: &ScheduleId) -> String {
    format!("schedule:{id}")
}

/// A running cluster node plus what the engine needs to decide ownership.
pub(crate) struct Cluster {
    node: Node,
    started: Instant,
    join_grace: Duration,
}

impl fmt::Debug for Cluster {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cluster")
            .field("node", &self.node)
            .field("join_grace", &self.join_grace)
            .finish()
    }
}

impl Cluster {
    /// Starts the node over `transport`, or over a UDP socket bound to
    /// `cluster.gossip_listen`.
    pub(crate) fn start(config: &Config, transport: Option<Arc<dyn Transport>>) -> Result<Cluster> {
        let api_url = config.cluster.api_url(&config.server);
        let membership = config
            .cluster
            .membership(&api_url)
            .map_err(|e| Error::io("resolving the cluster seeds", e))?;
        let transport: Arc<dyn Transport> = match transport {
            Some(t) => t,
            None => {
                let listen = &config.cluster.gossip_listen;
                let udp = UdpTransport::bind(listen.as_str())
                    .map_err(|e| Error::io(format!("binding the gossip socket {listen}"), e))?;
                Arc::new(udp)
            }
        };
        let node = Node::start(membership, transport)?;
        Ok(Cluster {
            node,
            started: Instant::now(),
            join_grace: config.cluster.join_grace,
        })
    }

    pub(crate) fn node_id(&self) -> &str {
        self.node.id().as_str()
    }

    pub(crate) fn ownership(&self, key: &str) -> Ownership {
        match self.node.owner_of(key) {
            Some(owner) if owner.id != *self.node.id() => Ownership::Remote(RemoteOwner {
                node: owner.id.into_string(),
                addr: owner.addr,
            }),
            // Owned here, or the view is empty because the node is
            // stopping: either way nobody else is better placed.
            _ => Ownership::Local,
        }
    }

    /// Whether schedules may fire here at all: the node has joined, or
    /// has waited out its grace period.
    pub(crate) fn may_fire(&self) -> bool {
        self.node.is_running()
            && (self.node.state() == belay_cluster::LocalState::Active
                || self.started.elapsed() >= self.join_grace)
    }

    /// Whether this node should fire schedule `id` now.
    pub(crate) fn fires(&self, id: &ScheduleId) -> bool {
        self.node.is_owner(&schedule_key(id))
    }

    pub(crate) fn live_count(&self) -> usize {
        self.node.live_members().len()
    }

    pub(crate) fn is_joined(&self) -> bool {
        self.node.state() == belay_cluster::LocalState::Active
    }

    pub(crate) fn members(&self) -> Vec<MemberInfo> {
        let leader = self.node.leader().map(|m| m.id);
        self.node
            .members()
            .into_iter()
            .map(|m| MemberInfo {
                leader: leader.as_ref() == Some(&m.id),
                state: match m.state {
                    MemberState::Alive => ApiMemberState::Alive,
                    MemberState::Suspect => ApiMemberState::Suspect,
                    MemberState::Dead => ApiMemberState::Dead,
                    MemberState::Left => ApiMemberState::Left,
                },
                id: m.id.into_string(),
                addr: m.addr,
            })
            .collect()
    }

    /// Announces the departure and stops the node.
    pub(crate) fn leave(&self) {
        if self.node.is_running() {
            self.node.leave();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_name_their_kind() {
        assert_eq!(queue_key(&QueueName::from_static("emails")), "queue:emails");
        assert_eq!(
            schedule_key(&ScheduleId::from_static("nightly")),
            "schedule:nightly"
        );
    }

    #[test]
    fn remote_owner_urls_join_cleanly() {
        let owner = RemoteOwner {
            node: "b".into(),
            addr: "http://10.0.0.6:7700/".into(),
        };
        assert_eq!(
            owner.url("/v1/jobs?queue=a"),
            "http://10.0.0.6:7700/v1/jobs?queue=a"
        );
        assert_eq!(owner.to_string(), "b (http://10.0.0.6:7700/)");
        assert!(Ownership::Local.is_local());
        assert!(!Ownership::Remote(owner).is_local());
    }
}
