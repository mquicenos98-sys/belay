//! [`TestWorker`] and its builder: the belay-worker runtime over the typed
//! client, behind a link the test can cut and whose calls it can count.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use belay_client::Client;
use belay_common::sync::lock;
use belay_common::BackoffPolicy;
use belay_proto::api::{
    ApiError, CompleteRequest, FailRequest, HeartbeatRequest, HeartbeatResponse, LeaseRequest,
    LeasedJob,
};
use belay_proto::{JobId, JobSource, QueueName, WorkerId};
use belay_worker::{
    Handler, KindOptions, Registry, Worker, WorkerConfig, WorkerHandle, WorkerReport, WorkerStats,
};

use crate::endpoint::{connect, Endpoint};
use crate::error::{Error, Result};
use crate::handlers::{Behavior, Gate, Recorder, RecordingHandler};
use crate::wait::eventually;

/// Builds a [`TestWorker`]; start with [`TestServer::worker`] or
/// [`TestWorker::builder`].
///
/// [`TestServer::worker`]: crate::TestServer::worker
#[must_use]
pub struct TestWorkerBuilder {
    endpoint: Endpoint,
    token: Option<String>,
    config: WorkerConfig,
    handlers: Vec<(String, Arc<dyn Handler>, KindOptions)>,
    recorder: Recorder,
    invalid: Option<Error>,
}

impl TestWorkerBuilder {
    pub(crate) fn new(endpoint: Endpoint) -> TestWorkerBuilder {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let id = WorkerId::new(format!("test-worker-{n}"))
            .expect("test-worker-<n> is a valid worker id");
        let config = WorkerConfig {
            id,
            queues: Vec::new(),
            concurrency: 4,
            lease_for: Duration::from_secs(30),
            poll_wait: Duration::from_millis(50),
            poll_interval: Duration::from_millis(5),
            heartbeat_every: Some(Duration::from_millis(50)),
            shutdown_grace: Duration::from_secs(1),
            cancel_grace: Duration::from_millis(200),
            source_backoff: BackoffPolicy::constant(Duration::from_millis(20)),
            report_attempts: 3,
            logger: None,
        };
        TestWorkerBuilder {
            endpoint,
            token: None,
            config,
            handlers: Vec::new(),
            recorder: Recorder::new(),
            invalid: None,
        }
    }

    /// The worker's id (default `test-worker-<n>`).
    pub fn id(mut self, id: &str) -> TestWorkerBuilder {
        match WorkerId::new(id) {
            Ok(id) => self.config.id = id,
            Err(e) => self.reject(format!("worker id {id:?}: {e}")),
        }
        self
    }

    /// Adds a queue to lease from; queues are tried in the order added.
    pub fn queue(mut self, name: &str) -> TestWorkerBuilder {
        match QueueName::new(name) {
            Ok(queue) => self.config.queues.push(queue),
            Err(e) => self.reject(format!("queue {name:?}: {e}")),
        }
        self
    }

    /// Adds several queues.
    pub fn queues<'a>(self, names: impl IntoIterator<Item = &'a str>) -> TestWorkerBuilder {
        names.into_iter().fold(self, TestWorkerBuilder::queue)
    }

    /// The bearer token to present.
    pub fn token(mut self, token: &str) -> TestWorkerBuilder {
        self.token = Some(token.to_owned());
        self
    }

    /// Executor threads (default 4).
    pub fn concurrency(mut self, n: usize) -> TestWorkerBuilder {
        self.config.concurrency = n;
        self
    }

    /// Lease length asked for (default 30 s, on the server's clock).
    pub fn lease_for(mut self, d: Duration) -> TestWorkerBuilder {
        self.config.lease_for = d;
        self
    }

    /// Heartbeat period (default 50 ms, so cancellation arrives quickly).
    pub fn heartbeat_every(mut self, d: Duration) -> TestWorkerBuilder {
        self.config.heartbeat_every = Some(d);
        self
    }

    /// Long-poll wait of each lease call (default 50 ms).
    pub fn poll_wait(mut self, d: Duration) -> TestWorkerBuilder {
        self.config.poll_wait = d;
        self
    }

    /// Any other [`WorkerConfig`] setting.
    pub fn configure(mut self, f: impl FnOnce(&mut WorkerConfig)) -> TestWorkerBuilder {
        f(&mut self.config);
        self
    }

    /// Records into `recorder` instead of a fresh one; share one recorder
    /// between workers to check properties across all of them.
    pub fn recorder(mut self, recorder: &Recorder) -> TestWorkerBuilder {
        self.recorder = recorder.clone();
        self
    }

    /// Runs jobs of `kind` with `behavior`, recording each attempt.
    pub fn behavior(mut self, kind: &str, behavior: Behavior) -> TestWorkerBuilder {
        let handler = RecordingHandler::new(self.recorder.clone(), behavior);
        self.handlers
            .push((kind.to_owned(), Arc::new(handler), KindOptions::new()));
        self
    }

    /// Succeeds at once, with the payload as the result.
    pub fn recording(self, kind: &str) -> TestWorkerBuilder {
        self.behavior(kind, Behavior::Succeed)
    }

    /// Fails retryably the first `n` times it sees a job, then succeeds.
    pub fn failing_n_times(self, kind: &str, n: u32) -> TestWorkerBuilder {
        self.behavior(kind, Behavior::FailTimes(n))
    }

    /// Fails every attempt non-retryably.
    pub fn failing(self, kind: &str) -> TestWorkerBuilder {
        self.behavior(kind, Behavior::FailAlways)
    }

    /// Sleeps for `d` (or until cancelled), then succeeds.
    pub fn sleeping(self, kind: &str, d: Duration) -> TestWorkerBuilder {
        self.behavior(kind, Behavior::Sleep(d))
    }

    /// Waits for `gate` to open (or for cancellation), then succeeds.
    pub fn gated(self, kind: &str, gate: &Gate) -> TestWorkerBuilder {
        self.behavior(kind, Behavior::Gated(gate.clone()))
    }

    /// Fails retryably with probability `probability`, deterministically
    /// per job and attempt (see [`Behavior::flaky_fails`]).
    pub fn flaky(self, kind: &str, probability: f64, seed: u64) -> TestWorkerBuilder {
        self.behavior(kind, Behavior::Flaky { probability, seed })
    }

    /// Runs jobs of `kind` with a handler of the test's own; it is not
    /// recorded.
    pub fn handler(mut self, kind: &str, handler: impl Handler + 'static) -> TestWorkerBuilder {
        self.handlers
            .push((kind.to_owned(), Arc::new(handler), KindOptions::new()));
        self
    }

    /// Per-kind limits for a kind added before.
    pub fn kind_options(mut self, kind: &str, options: KindOptions) -> TestWorkerBuilder {
        match self.handlers.iter_mut().find(|(k, _, _)| k == kind) {
            Some(entry) => entry.2 = options,
            None => self.reject(format!("no handler for kind {kind:?} to set options on")),
        }
        self
    }

    /// Starts the worker; it begins leasing at once.
    pub fn start(self) -> Result<TestWorker> {
        if let Some(e) = self.invalid {
            return Err(e);
        }
        if self.config.queues.is_empty() {
            return Err(Error::Invalid(
                "a test worker needs at least one queue".to_owned(),
            ));
        }
        let registry = self
            .handlers
            .into_iter()
            .fold(Registry::new(), |registry, (kind, handler, options)| {
                registry.register_with(kind, handler, options)
            });
        let link = Arc::new(Link {
            endpoint: self.endpoint,
            token: self.token,
            client: Mutex::new(None),
            connected: AtomicBool::new(true),
            lease_calls: AtomicU64::new(0),
            empty_leases: AtomicU64::new(0),
            failed_calls: AtomicU64::new(0),
        });
        let id = self.config.id.clone();
        let source: Arc<dyn JobSource> = link.clone();
        let handle = Worker::new(source, registry, self.config)?.spawn()?;
        Ok(TestWorker {
            id,
            handle: Some(handle),
            link,
            recorder: self.recorder,
        })
    }

    fn reject(&mut self, message: String) {
        if self.invalid.is_none() {
            self.invalid = Some(Error::Invalid(message));
        }
    }
}

impl fmt::Debug for TestWorkerBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kinds: Vec<&str> = self.handlers.iter().map(|(k, _, _)| k.as_str()).collect();
        f.debug_struct("TestWorkerBuilder")
            .field("url", &self.endpoint.url())
            .field("id", &self.config.id)
            .field("queues", &self.config.queues)
            .field("kinds", &kinds)
            .finish_non_exhaustive()
    }
}

/// The real belay-worker runtime, leasing over HTTP through the typed
/// client and running [`RecordingHandler`]s. Dropping it stops it and
/// waits for its threads.
///
/// Between the worker and the client sits a link that the test can cut
/// ([`disconnect`](TestWorker::disconnect)): every call then fails as if
/// the network were down, which is how a test makes a worker vanish with
/// jobs leased ([`crash`](TestWorker::crash)). The link also counts lease
/// calls, so a test can wait until a worker has asked for work and been
/// told there is none ([`wait_for_empty_leases`](TestWorker::wait_for_empty_leases))
/// instead of sleeping for an arbitrary time.
pub struct TestWorker {
    id: WorkerId,
    handle: Option<WorkerHandle>,
    link: Arc<Link>,
    recorder: Recorder,
}

impl TestWorker {
    /// A builder for a worker talking to the server at `url`.
    pub fn builder(url: &str) -> TestWorkerBuilder {
        TestWorkerBuilder::new(Endpoint::new(url))
    }

    /// The worker's id, as the server sees it.
    #[must_use]
    pub fn id(&self) -> &WorkerId {
        &self.id
    }

    /// What the worker's handlers recorded.
    #[must_use]
    pub fn recorder(&self) -> &Recorder {
        &self.recorder
    }

    /// The runtime's own counters.
    #[must_use]
    pub fn stats(&self) -> WorkerStats {
        self.handle
            .as_ref()
            .map_or_else(WorkerStats::default, WorkerHandle::stats)
    }

    /// Lease calls made so far.
    #[must_use]
    pub fn lease_calls(&self) -> u64 {
        self.link.lease_calls.load(Ordering::SeqCst)
    }

    /// Lease calls that came back without a job.
    #[must_use]
    pub fn empty_leases(&self) -> u64 {
        self.link.empty_leases.load(Ordering::SeqCst)
    }

    /// Calls to the server that failed, including those refused while
    /// disconnected.
    #[must_use]
    pub fn failed_calls(&self) -> u64 {
        self.link.failed_calls.load(Ordering::SeqCst)
    }

    /// Waits until the worker has asked for work `n` more times and got
    /// none: proof that it had free slots and the server held jobs back.
    pub fn wait_for_empty_leases(&self, n: u64, timeout: Duration) -> Result<()> {
        let target = self.empty_leases() + n;
        eventually(timeout, || self.empty_leases() >= target).map_err(|_| {
            Error::timeout(
                format!("worker {} to be refused work {n} more times", self.id),
                timeout,
            )
        })
    }

    /// Waits until `n` more calls to the server have failed.
    pub fn wait_for_failed_calls(&self, n: u64, timeout: Duration) -> Result<()> {
        let target = self.failed_calls() + n;
        eventually(timeout, || self.failed_calls() >= target)
            .map_err(|_| Error::timeout(format!("{n} more failed calls from {}", self.id), timeout))
    }

    /// Cuts the worker off: every call fails with `unavailable` until
    /// [`reconnect`](Self::reconnect). A lease call already in flight
    /// still returns what the server hands it.
    pub fn disconnect(&self) {
        self.link.connected.store(false, Ordering::SeqCst);
    }

    /// Restores the link.
    pub fn reconnect(&self) {
        self.link.connected.store(true, Ordering::SeqCst);
    }

    /// Whether the link is up.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.link.connected.load(Ordering::SeqCst)
    }

    /// Stops gracefully: running jobs get the shutdown grace period (1 s)
    /// and unstarted ones are handed back.
    pub fn stop(mut self) -> WorkerReport {
        self.finish()
    }

    /// Vanishes: disconnects, then stops. The server hears nothing more,
    /// so the jobs it leased stay leased until their leases expire.
    pub fn crash(mut self) -> WorkerReport {
        self.disconnect();
        self.finish()
    }

    fn finish(&mut self) -> WorkerReport {
        self.handle
            .take()
            .map_or_else(WorkerReport::default, WorkerHandle::stop)
    }
}

impl Drop for TestWorker {
    fn drop(&mut self) {
        self.finish();
    }
}

impl fmt::Debug for TestWorker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestWorker")
            .field("id", &self.id)
            .field("connected", &self.is_connected())
            .field("lease_calls", &self.lease_calls())
            .field("recorder", &self.recorder)
            .finish()
    }
}

/// The worker's [`JobSource`]: the typed client, behind a switch.
struct Link {
    endpoint: Endpoint,
    token: Option<String>,
    /// The client for the URL it was built for; rebuilt if the server
    /// moved.
    client: Mutex<Option<(String, Client)>>,
    connected: AtomicBool,
    lease_calls: AtomicU64,
    empty_leases: AtomicU64,
    failed_calls: AtomicU64,
}

impl Link {
    fn client(&self) -> Result<Client, ApiError> {
        if !self.connected.load(Ordering::SeqCst) {
            self.failed_calls.fetch_add(1, Ordering::SeqCst);
            return Err(ApiError::unavailable(
                "the test harness has cut this worker off from the server",
            ));
        }
        let url = self.endpoint.url();
        let mut cached = lock(&self.client);
        if let Some((built_for, client)) = cached.as_ref() {
            if *built_for == url {
                return Ok(client.clone());
            }
        }
        let client = connect(&url, self.token.as_deref())
            .map_err(|e| ApiError::invalid_argument(format!("test worker client: {e}")))?;
        *cached = Some((url, client.clone()));
        Ok(client)
    }

    fn count<T>(&self, result: Result<T, ApiError>) -> Result<T, ApiError> {
        if result.is_err() {
            self.failed_calls.fetch_add(1, Ordering::SeqCst);
        }
        result
    }
}

impl JobSource for Link {
    fn lease(&self, req: &LeaseRequest) -> Result<Vec<LeasedJob>, ApiError> {
        let client = self.client()?;
        self.lease_calls.fetch_add(1, Ordering::SeqCst);
        let leased = self.count(JobSource::lease(&client, req))?;
        if leased.is_empty() {
            self.empty_leases.fetch_add(1, Ordering::SeqCst);
        }
        Ok(leased)
    }

    fn heartbeat(&self, id: &JobId, req: &HeartbeatRequest) -> Result<HeartbeatResponse, ApiError> {
        let client = self.client()?;
        self.count(JobSource::heartbeat(&client, id, req))
    }

    fn complete(&self, id: &JobId, req: &CompleteRequest) -> Result<(), ApiError> {
        let client = self.client()?;
        self.count(JobSource::complete(&client, id, req))
    }

    fn fail(&self, id: &JobId, req: &FailRequest) -> Result<(), ApiError> {
        let client = self.client()?;
        self.count(JobSource::fail(&client, id, req))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builders_reject_bad_names_at_start() {
        let err = TestWorker::builder("http://127.0.0.1:1")
            .queue("Bad Queue")
            .recording("k")
            .start()
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(ref m) if m.starts_with("queue \"Bad Queue\"")));
        let err = TestWorker::builder("http://127.0.0.1:1")
            .recording("k")
            .start()
            .unwrap_err();
        assert!(err.to_string().contains("at least one queue"), "{err}");
        let err = TestWorker::builder("http://127.0.0.1:1")
            .queue("q")
            .kind_options("missing", KindOptions::new().concurrency(1))
            .start()
            .unwrap_err();
        assert!(err.to_string().contains("\"missing\""), "{err}");
    }

    #[test]
    fn a_disconnected_link_refuses_every_call() {
        let worker = TestWorker::builder("http://127.0.0.1:1")
            .queue("q")
            .recording("k")
            .start()
            .unwrap();
        worker.disconnect();
        assert!(!worker.is_connected());
        worker
            .wait_for_failed_calls(2, Duration::from_secs(5))
            .unwrap();
        assert_eq!(worker.lease_calls(), 0);
        worker.reconnect();
        assert!(worker.is_connected());
        let report = worker.stop();
        assert!(report.drained);
        assert_eq!(report.stats.leased, 0);
    }
}
