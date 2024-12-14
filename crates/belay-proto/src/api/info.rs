//! Smaller response bodies: server and worker information, cluster
//! members, and the results of admin, purge and schedule actions.
//!
//! All of these are produced by the server, so decoding ignores unknown
//! fields.

use std::time::Duration;

use belay_common::Timestamp;
use belay_json::{FromJson, FromJsonError, ObjectBuilder, ObjectReader, ToJson, Value};

use crate::ids::{JobId, QueueName, WorkerId};
use crate::wire::{text_enum, timestamp_from_json, timestamp_to_json, BuilderExt, ReaderExt};

/// `GET /v1/info`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerInfo {
    /// belay version, e.g. `0.6.0`.
    pub version: String,
    /// This node's id.
    pub node: String,
    /// When the process started.
    pub started_at: Timestamp,
    /// Time since start.
    pub uptime: Duration,
    /// The server's clock when answering; compare with your own to spot
    /// skew, which matters for `run_at` and lease deadlines.
    pub time: Timestamp,
    /// `memory` or `disk`.
    pub storage: String,
    /// Live cluster members, 1 when not clustered.
    pub cluster_size: u32,
}

impl ToJson for ServerInfo {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("version", &self.version)
            .field("node", &self.node)
            .timestamp("started_at", self.started_at)
            .duration("uptime", self.uptime)
            .timestamp("time", self.time)
            .field("storage", &self.storage)
            .field("cluster_size", self.cluster_size)
            .build()
    }
}

impl FromJson for ServerInfo {
    fn from_json(value: &Value) -> Result<ServerInfo, FromJsonError> {
        let r = ObjectReader::new(value, "ServerInfo")?;
        Ok(ServerInfo {
            version: r.req("version")?,
            node: r.req("node")?,
            started_at: r.req_timestamp("started_at")?,
            uptime: r.req_duration("uptime")?,
            time: r.req_timestamp("time")?,
            storage: r.or_default("storage")?,
            cluster_size: r.or("cluster_size", 1)?,
        })
    }
}

/// A worker as seen by the server (`GET /v1/workers`), built from its lease
/// and heartbeat traffic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerInfo {
    /// The worker.
    pub id: WorkerId,
    /// Queues named in its latest lease request.
    pub queues: Vec<QueueName>,
    /// Remote address of its latest request, when known.
    pub addr: Option<String>,
    /// First request seen.
    pub first_seen: Timestamp,
    /// Latest request seen.
    pub last_seen: Timestamp,
    /// Jobs it holds leases on now.
    pub active_jobs: u32,
    /// Jobs leased since first seen.
    pub leased: u64,
    /// Jobs it completed.
    pub succeeded: u64,
    /// Attempts it failed (including expired leases).
    pub failed: u64,
}

impl WorkerInfo {
    /// A worker first seen at `now`.
    #[must_use]
    pub fn new(id: WorkerId, now: Timestamp) -> WorkerInfo {
        WorkerInfo {
            id,
            queues: Vec::new(),
            addr: None,
            first_seen: now,
            last_seen: now,
            active_jobs: 0,
            leased: 0,
            succeeded: 0,
            failed: 0,
        }
    }

    /// Whether nothing has been heard from it for `timeout` or longer.
    #[must_use]
    pub fn is_lost(&self, now: Timestamp, timeout: Duration) -> bool {
        now.saturating_duration_since(self.last_seen) >= timeout
    }
}

impl ToJson for WorkerInfo {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("id", &self.id)
            .field("queues", &self.queues)
            .opt_field("addr", &self.addr)
            .timestamp("first_seen", self.first_seen)
            .timestamp("last_seen", self.last_seen)
            .field("active_jobs", self.active_jobs)
            .field("leased", self.leased)
            .field("succeeded", self.succeeded)
            .field("failed", self.failed)
            .build()
    }
}

impl FromJson for WorkerInfo {
    fn from_json(value: &Value) -> Result<WorkerInfo, FromJsonError> {
        let r = ObjectReader::new(value, "WorkerInfo")?;
        Ok(WorkerInfo {
            id: r.req("id")?,
            queues: r.or_default("queues")?,
            addr: r.opt("addr")?,
            first_seen: r.req_timestamp("first_seen")?,
            last_seen: r.req_timestamp("last_seen")?,
            active_jobs: r.or_default("active_jobs")?,
            leased: r.or_default("leased")?,
            succeeded: r.or_default("succeeded")?,
            failed: r.or_default("failed")?,
        })
    }
}

text_enum! {
    /// A cluster member's health, as the failure detector sees it.
    MemberState, "member state" {
        /// Responding to probes.
        Alive = "alive",
        /// Missed probes; may be declared dead soon.
        Suspect = "suspect",
        /// Declared dead; its queues moved to other nodes.
        Dead = "dead",
        /// Left the cluster gracefully.
        Left = "left",
    }
}

/// One entry of `GET /v1/cluster/members`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberInfo {
    /// Node id.
    pub id: String,
    /// Its HTTP API address.
    pub addr: String,
    /// Health.
    pub state: MemberState,
    /// Whether it runs cluster-wide chores (the lowest live node id).
    pub leader: bool,
}

impl ToJson for MemberInfo {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("id", &self.id)
            .field("addr", &self.addr)
            .field("state", self.state)
            .field("leader", self.leader)
            .build()
    }
}

impl FromJson for MemberInfo {
    fn from_json(value: &Value) -> Result<MemberInfo, FromJsonError> {
        let r = ObjectReader::new(value, "MemberInfo")?;
        Ok(MemberInfo {
            id: r.req("id")?,
            addr: r.req("addr")?,
            state: r.req("state")?,
            leader: r.or_default("leader")?,
        })
    }
}

/// `POST /v1/queues/:name/purge` response.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PurgeResponse {
    /// Jobs removed.
    pub purged: u64,
}

impl ToJson for PurgeResponse {
    fn to_json(&self) -> Value {
        ObjectBuilder::new().field("purged", self.purged).build()
    }
}

impl FromJson for PurgeResponse {
    fn from_json(value: &Value) -> Result<PurgeResponse, FromJsonError> {
        let r = ObjectReader::new(value, "PurgeResponse")?;
        Ok(PurgeResponse {
            purged: r.req("purged")?,
        })
    }
}

/// `POST /v1/schedules/:id/trigger` response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TriggerResponse {
    /// The job enqueued by the manual fire.
    pub job: JobId,
}
