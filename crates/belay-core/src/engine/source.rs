//! The worker protocol in-process: [`Engine`] implements
//! [`JobSource`], and [`Engine::embedded_worker`] runs a belay-worker
//! [`Worker`] against it without HTTP in between (`belay dev`).

use std::sync::Arc;

use belay_proto::api::{
    ApiError, CompleteRequest, FailRequest, HeartbeatRequest, HeartbeatResponse, LeaseRequest,
    LeaseResponse, LeasedJob,
};
use belay_proto::{JobId, JobSource};
use belay_worker::{Registry, Worker, WorkerConfig, WorkerHandle};

use super::Engine;
use crate::error::Result;

/// `POST /v1/lease` and the per-job worker calls. Leasing long-polls up to
/// the request's `wait`; it returns at once, empty, while the engine shuts
/// down.
impl JobSource for Engine {
    fn lease(&self, req: &LeaseRequest) -> Result<Vec<LeasedJob>, ApiError> {
        Ok(self.shared.queue.lease(req)?)
    }

    fn heartbeat(&self, id: &JobId, req: &HeartbeatRequest) -> Result<HeartbeatResponse, ApiError> {
        Ok(self.shared.queue.heartbeat(id, req)?)
    }

    fn complete(&self, id: &JobId, req: &CompleteRequest) -> Result<(), ApiError> {
        Ok(self.shared.queue.complete(id, req)?)
    }

    fn fail(&self, id: &JobId, req: &FailRequest) -> Result<(), ApiError> {
        self.shared.queue.fail(id, req)?;
        Ok(())
    }
}
