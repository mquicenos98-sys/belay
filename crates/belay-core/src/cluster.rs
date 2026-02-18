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
