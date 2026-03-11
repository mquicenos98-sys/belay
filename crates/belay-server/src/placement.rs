//! Which node serves a request, in a cluster.
//!
//! Every queue and every schedule is owned by one live node (rendezvous
//! hashing over the members, see belay-cluster). A request about a queue
//! or schedule this node does not own is answered with `307 Temporary
//! Redirect` to the same path and query on the owner; 307 keeps the method
//! and body, so a redirected `POST` is replayed as is. Nothing is proxied:
//! the client follows the redirect with its own credentials, which every
//! node accepts alike.
//!
//! Requests naming several queues are split by owner:
//!
//! - an enqueue batch must be owned by a single node, since the batch is
//!   atomic and nodes share no storage: all local is served, all on one
//!   other node is redirected there, anything else is refused with `400`
//!   listing the owners, so the client can split it;
//! - a lease is served from the queues owned here, and the ones owned
//!   elsewhere are skipped and named in the `Belay-Remote-Queues` response
//!   header. Only when none is owned here is it redirected, to the owner
//!   of the first (most preferred) queue. A worker listing queues that
//!   live on different nodes therefore needs a connection to each of them.
//!
//! Without clustering every key is local and none of this costs anything.

use belay_core::{Engine, Ownership, RemoteOwner};
use belay_http::header::LOCATION;
use belay_http::{Request, Response, StatusCode};
use belay_json::{json, Value};
use belay_proto::api::ApiError;
use belay_proto::{QueueName, ScheduleId};

use crate::reply;

/// The response header of `POST /v1/lease` naming (comma-separated) the
/// requested queues that were skipped because other cluster nodes own them.
pub const REMOTE_QUEUES_HEADER: &str = "Belay-Remote-Queues";

/// Where a set of queues is served.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Split {
    /// All owned here.
    Local,
    /// All owned by one other node.
    Remote(RemoteOwner),
    /// Spread over several nodes.
    Mixed {
        /// The queues owned here, in the order given.
        local: Vec<QueueName>,
        /// The others with their owners, in the order given.
        remote: Vec<(QueueName, RemoteOwner)>,
    },
}
