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

impl Engine {
    /// Leases jobs, wrapped as the `POST /v1/lease` response body.
    pub fn lease_response(&self, req: &LeaseRequest) -> Result<LeaseResponse, ApiError> {
        Ok(LeaseResponse {
            jobs: JobSource::lease(self, req)?,
        })
    }

    /// Starts a worker that leases from this engine in-process and runs
    /// the handlers in `registry`.
    ///
    /// The worker talks to the queue directly, so it keeps working however
    /// the engine is shared. Stop it (with [`WorkerHandle::stop`]) before
    /// shutting the engine down: once the queue stops handing out jobs the
    /// worker only backs off and retries.
    ///
    /// ```
    /// use std::time::Duration;
    /// use belay_core::{Config, Engine};
    /// use belay_proto::{JobSpec, JobState};
    /// use belay_worker::{builtin, WorkerConfig};
    ///
    /// let engine = Engine::start(Config::dev())?;
    /// let id = engine.enqueue(JobSpec::new("default", "echo"))?.id;
    /// let worker = engine.embedded_worker(
    ///     builtin::defaults(),
    ///     WorkerConfig::default()
    ///         .queues(vec!["default".parse()?])
    ///         .poll_wait(Duration::from_millis(50)),
    /// )?;
    /// let deadline = std::time::Instant::now() + Duration::from_secs(10);
    /// while engine.get_job(&id)?.state != JobState::Succeeded && std::time::Instant::now() < deadline {
    ///     std::thread::sleep(Duration::from_millis(5));
    /// }
    /// assert_eq!(engine.get_job(&id)?.state, JobState::Succeeded);
    /// worker.stop();
    /// engine.shutdown();
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn embedded_worker(
        &self,
        registry: Registry,
        config: WorkerConfig,
    ) -> Result<WorkerHandle> {
        let source: Arc<dyn JobSource> = self.shared.queue.clone();
        Ok(Worker::new(source, registry, config)?.spawn()?)
    }
}
