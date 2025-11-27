//! Server-wide calls: identity, probes, metrics, workers, cluster
//! membership, and the admin endpoints.

use belay_json::Value;
use belay_proto::api::{CheckpointResponse, ErrorCode, MemberInfo, ServerInfo, WorkerInfo};

use crate::client::{Call, Client};
use crate::error::{Error, Result};
use crate::pager::collect_all;
use crate::retry::Safety;

impl Client {
    /// Version, node id, uptime and clock of the server (`GET /v1/info`).
    pub fn info(&self) -> Result<ServerInfo> {
        self.fetch(Call::get("/v1/info".to_owned()))
    }

    /// Liveness probe (`GET /healthz`): `Ok` when the process answers.
    /// Never retried; a probe reports what it sees.
    pub fn health(&self) -> Result<()> {
        self.execute(probe("/healthz"))
    }

    /// Readiness probe (`GET /readyz`): `true` when the server accepts
    /// work, `false` when it answers `503` (starting, recovering storage,
    /// shutting down). Other failures are errors. Never retried.
    pub fn ready(&self) -> Result<bool> {
        match self.execute(probe("/readyz")) {
            Ok(()) => Ok(true),
            Err(Error::Api(e)) if e.code == ErrorCode::Unavailable => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// The Prometheus text exposition of the server's metrics
    /// (`GET /metrics`).
    pub fn metrics(&self) -> Result<String> {
        self.fetch_text(Call::get("/metrics".to_owned()).accept("text/plain"))
    }

    /// Workers the server has heard from recently, built from their lease
    /// and heartbeat traffic (`GET /v1/workers`).
    pub fn workers(&self) -> Result<Vec<WorkerInfo>> {
        collect_all(self, "/v1/workers")
    }

    /// The cluster's members as this node sees them
    /// (`GET /v1/cluster/members`); a single entry when not clustered.
    pub fn members(&self) -> Result<Vec<MemberInfo>> {
        collect_all(self, "/v1/cluster/members")
    }
}

fn probe(path: &str) -> Call {
    Call::get(path.to_owned())
        .safety(Safety::Never)
        .accept("text/plain, application/json")
}
