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
    /// Whether it has the lowest live node id. Reported only; nothing
    /// depends on it yet.
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

impl ToJson for TriggerResponse {
    fn to_json(&self) -> Value {
        ObjectBuilder::new().field("job", self.job).build()
    }
}

impl FromJson for TriggerResponse {
    fn from_json(value: &Value) -> Result<TriggerResponse, FromJsonError> {
        let r = ObjectReader::new(value, "TriggerResponse")?;
        Ok(TriggerResponse { job: r.req("job")? })
    }
}

/// `GET /v1/schedules/:id/preview?n=` response: upcoming fire times,
/// with calendars and start/end bounds applied, before jitter.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PreviewResponse {
    /// Fire times, earliest first.
    pub times: Vec<Timestamp>,
}

impl ToJson for PreviewResponse {
    fn to_json(&self) -> Value {
        let times = self.times.iter().map(|t| timestamp_to_json(*t)).collect();
        ObjectBuilder::new()
            .raw("times", Value::Array(times))
            .build()
    }
}

impl FromJson for PreviewResponse {
    fn from_json(value: &Value) -> Result<PreviewResponse, FromJsonError> {
        let r = ObjectReader::new(value, "PreviewResponse")?;
        let times = r.req_with("times", |v| {
            let items = v
                .as_array()
                .ok_or_else(|| FromJsonError::expected("array of timestamps", v))?;
            items
                .iter()
                .enumerate()
                .map(|(i, t)| timestamp_from_json(t).map_err(|e| e.at_index(i)))
                .collect()
        })?;
        Ok(PreviewResponse { times })
    }
}

/// `POST /v1/admin/checkpoint` response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CheckpointResponse {
    /// Store sequence number the snapshot covers.
    pub seq: u64,
    /// How long writing it took.
    pub took: Duration,
}

impl ToJson for CheckpointResponse {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("seq", self.seq)
            .duration("took", self.took)
            .build()
    }
}

impl FromJson for CheckpointResponse {
    fn from_json(value: &Value) -> Result<CheckpointResponse, FromJsonError> {
        let r = ObjectReader::new(value, "CheckpointResponse")?;
        Ok(CheckpointResponse {
            seq: r.req("seq")?,
            took: r.req_duration("took")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_json::json;

    fn t(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn server_info_round_trips_and_ignores_new_fields() {
        let info = ServerInfo {
            version: "0.6.0".into(),
            node: "node-a".into(),
            started_at: t("2025-03-04T00:00:00Z"),
            uptime: Duration::from_secs(3_723),
            time: t("2025-03-04T01:02:03Z"),
            storage: "disk".into(),
            cluster_size: 3,
        };
        let mut v = info.to_json();
        assert_eq!(v["uptime"], "1h2m3s");
        assert_eq!(ServerInfo::from_json(&v), Ok(info.clone()));
        v.as_object_mut().unwrap().insert("build", "abc123");
        assert_eq!(ServerInfo::from_json(&v), Ok(info));
    }

    #[test]
    fn worker_info_round_trips_and_detects_loss() {
        let now = t("2025-03-04T00:00:00Z");
        let mut w = WorkerInfo::new(WorkerId::from_static("web-1:4121"), now);
        w.queues = vec![QueueName::from_static("emails")];
        w.addr = Some("10.0.0.7:51234".into());
        w.active_jobs = 2;
        w.leased = 10;
        w.succeeded = 7;
        w.failed = 1;
        assert_eq!(WorkerInfo::from_json(&w.to_json()), Ok(w.clone()));
        assert!(!w.is_lost(now + Duration::from_secs(59), Duration::from_secs(60)));
        assert!(w.is_lost(now + Duration::from_secs(60), Duration::from_secs(60)));
    }

    #[test]
    fn small_responses_round_trip() {
        let m = MemberInfo {
            id: "node-b".into(),
            addr: "10.0.0.2:7070".into(),
            state: MemberState::Suspect,
            leader: false,
        };
        assert_eq!(MemberInfo::from_json(&m.to_json()), Ok(m));
        let p = PurgeResponse { purged: 12 };
        assert_eq!(p.to_json(), json!({"purged": 12}));
        assert_eq!(PurgeResponse::from_json(&p.to_json()), Ok(p));
        let tr = TriggerResponse {
            job: "01HQ3K9Z8N6V2T4R5S7W0X1Y2Z".parse().unwrap(),
        };
        assert_eq!(TriggerResponse::from_json(&tr.to_json()), Ok(tr));
        let c = CheckpointResponse {
            seq: 991,
            took: Duration::from_millis(42),
        };
        assert_eq!(c.to_json(), json!({"seq": 991, "took": "42ms"}));
        assert_eq!(CheckpointResponse::from_json(&c.to_json()), Ok(c));
    }

    #[test]
    fn preview_times_are_rfc3339_with_indexed_errors() {
        let p = PreviewResponse {
            times: vec![t("2025-03-04T02:00:00Z"), t("2025-03-05T02:00:00Z")],
        };
        assert_eq!(
            p.to_json(),
            json!({"times": ["2025-03-04T02:00:00.000Z", "2025-03-05T02:00:00.000Z"]})
        );
        assert_eq!(PreviewResponse::from_json(&p.to_json()), Ok(p));
        let err =
            PreviewResponse::from_json(&json!({"times": ["2025-03-04T02:00:00Z", 5]})).unwrap_err();
        assert_eq!(
            err.to_string(),
            "$.times[1]: expected RFC 3339 timestamp string, found 5"
        );
    }
}
