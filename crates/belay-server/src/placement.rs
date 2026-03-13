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

/// Groups `queues` by owner, as reported by `owner_of`.
pub(crate) fn split<'q>(
    queues: impl IntoIterator<Item = &'q QueueName>,
    owner_of: impl Fn(&QueueName) -> Ownership,
) -> Split {
    let mut local = Vec::new();
    let mut remote: Vec<(QueueName, RemoteOwner)> = Vec::new();
    for queue in queues {
        if local.contains(queue) || remote.iter().any(|(q, _)| q == queue) {
            continue;
        }
        match owner_of(queue) {
            Ownership::Local => local.push(queue.clone()),
            Ownership::Remote(owner) => remote.push((queue.clone(), owner)),
        }
    }
    match remote.first() {
        None => Split::Local,
        Some((_, first)) if local.is_empty() && remote.iter().all(|(_, o)| o == first) => {
            Split::Remote(first.clone())
        }
        Some(_) => Split::Mixed { local, remote },
    }
}

/// The redirect for `req` if `queue` is owned elsewhere.
pub(crate) fn for_queue(engine: &Engine, req: &Request, queue: &QueueName) -> Option<Response> {
    remote(engine.owner_of(queue)).map(|owner| redirect(req, &owner))
}

/// The redirect for `req` if schedule `id` is owned elsewhere.
pub(crate) fn for_schedule(engine: &Engine, req: &Request, id: &ScheduleId) -> Option<Response> {
    remote(engine.owner_of_schedule(id)).map(|owner| redirect(req, &owner))
}

fn remote(ownership: Ownership) -> Option<RemoteOwner> {
    match ownership {
        Ownership::Local => None,
        Ownership::Remote(owner) => Some(owner),
    }
}

/// `307` to the same path and query on `owner`, with a small JSON body
/// for people reading the answer with curl.
pub(crate) fn redirect(req: &Request, owner: &RemoteOwner) -> Response {
    let location = owner.url(&path_and_query(req));
    let body = json!({"redirect": {"node": owner.node.as_str(), "location": location.as_str()}});
    reply::json(StatusCode::TEMPORARY_REDIRECT, &body).header(LOCATION, location)
}

/// The request's path and query as received, without any scheme and
/// authority a proxy may have sent in absolute form.
pub(crate) fn path_and_query(req: &Request) -> String {
    match req.path.split_once('?') {
        Some((_, query)) => format!("{}?{}", req.path, query.split('#').next().unwrap_or("")),
        None => req.path.clone(),
    }
}
