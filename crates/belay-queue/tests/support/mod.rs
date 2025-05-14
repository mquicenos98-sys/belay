//! Shared test harness: an engine on a manual clock with a collecting
//! event sink, plus shorthands for the worker protocol.
// Each test binary uses a different subset of these helpers.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use belay_common::log::{set_max_level, CaptureLogger, LevelFilter};
use belay_common::{ManualClock, Timestamp};
use belay_proto::api::{
    CompleteRequest, FailRequest, HeartbeatRequest, HeartbeatResponse, JobFilter, LeaseRequest,
    LeasedJob,
};
use belay_proto::source::CollectingSink;
use belay_proto::{Job, JobId, JobSpec, JobState, QueueName, WorkerId};
use belay_queue::{EngineOptions, QueueEngine, TickReport};
use belay_store::vfs::MemFs;
use belay_store::{
    CheckpointInfo, KeyRange, Kv, KvStats, LogKv, MemKv, Options, StoreError, SyncPolicy,
    WriteBatch,
};

/// Where every test clock starts: 2023-11-14T22:13:20Z.
pub fn start() -> Timestamp {
    Timestamp::from_secs(1_700_000_000)
}

pub fn q(name: &'static str) -> QueueName {
    QueueName::from_static(name)
}

pub fn w(name: &'static str) -> WorkerId {
    WorkerId::from_static(name)
}

/// Keeps engine and store log lines out of the test output.
pub fn quiet_logs() {
    set_max_level(LevelFilter::Off);
}

/// A durable store on an in-memory file system that syncs every write.
pub fn durable(fs: &MemFs) -> Arc<dyn Kv> {
    quiet_logs();
    let opts = Options::new()
        .fs(fs.clone())
        .sync(SyncPolicy::Always)
        .manual_checkpoints();
    Arc::new(LogKv::open(PathBuf::from("store"), opts).expect("opening the store"))
}

/// Drops a store whose file system crashed under it, keeping the expected
/// "closing store failed" warning out of the output.
pub fn drop_quietly<T>(value: T) {
    CaptureLogger::new().scope(move || drop(value));
}

pub struct Harness {
    pub clock: ManualClock,
    pub sink: Arc<CollectingSink>,
    pub kv: Arc<dyn Kv>,
    pub engine: Arc<QueueEngine>,
    pub opts: EngineOptions,
}

impl Harness {
    /// An engine on a fresh in-memory store, seeded for reproducibility.
    pub fn new() -> Harness {
        Harness::with_options(EngineOptions::new())
    }

    pub fn with_options(opts: EngineOptions) -> Harness {
        Harness::on(Arc::new(MemKv::new()), opts)
    }

    pub fn on(kv: Arc<dyn Kv>, opts: EngineOptions) -> Harness {
        quiet_logs();
        let opts = opts.seed(0x5eed);
        let clock = ManualClock::new(start());
        let sink = Arc::new(CollectingSink::new());
        let engine = QueueEngine::open(Arc::clone(&kv), clock.shared(), sink.clone(), opts.clone())
            .expect("opening the engine");
        Harness {
            clock,
            sink,
            kv,
            engine: Arc::new(engine),
            opts,
        }
    }

    /// Drops the engine and opens a new one on the same store and clock.
    pub fn reopen(&mut self) {
        self.reopen_on(Arc::clone(&self.kv));
    }

    /// Drops the engine and opens a new one on `kv` (e.g. a store reopened
    /// after a crash), keeping the clock.
    pub fn reopen_on(&mut self, kv: Arc<dyn Kv>) {
        let engine = QueueEngine::open(
            Arc::clone(&kv),
            self.clock.shared(),
            self.sink.clone(),
            self.opts.clone(),
        )
        .expect("reopening the engine");
        self.kv = kv;
        self.engine = Arc::new(engine);
    }

    /// Simulates a process restart: drops the engine together with its
    /// store (closing a durable one, or abandoning one whose file system
    /// crashed), then opens a new engine on the store `open` returns.
    pub fn restart(&mut self, open: impl FnOnce() -> Arc<dyn Kv>) {
        let stand_in: Arc<dyn Kv> = Arc::new(MemKv::new());
        let engine = QueueEngine::open(
            Arc::clone(&stand_in),
            self.clock.shared(),
            Arc::new(CollectingSink::new()),
            EngineOptions::new(),
        )
        .expect("opening a stand-in engine");
        let old_engine = std::mem::replace(&mut self.engine, Arc::new(engine));
        let old_kv = std::mem::replace(&mut self.kv, stand_in);
        drop_quietly((old_engine, old_kv));
        self.reopen_on(open());
    }

    pub fn now(&self) -> Timestamp {
        use belay_common::Clock;
        self.clock.now()
    }

    pub fn advance(&self, d: Duration) -> Timestamp {
        self.clock.advance(d)
    }

    pub fn secs(&self, n: u64) -> Timestamp {
        self.advance(Duration::from_secs(n))
    }

    pub fn tick(&self) -> TickReport {
        self.engine.tick(self.now()).expect("tick")
    }

    pub fn enqueue(&self, spec: JobSpec) -> JobId {
        self.engine.enqueue(spec).expect("enqueue").id
    }

    pub fn lease_from(
        &self,
        worker: &'static str,
        queues: &[&'static str],
        n: u32,
    ) -> Vec<LeasedJob> {
        let req = LeaseRequest::new(w(worker), queues.iter().map(|name| q(name)).collect())
            .max_jobs(n)
            .lease_for(Duration::from_secs(30));
        self.engine.lease(&req).expect("lease")
    }

    pub fn lease(&self, queue: &'static str, n: u32) -> Vec<LeasedJob> {
        self.lease_from("w1", &[queue], n)
    }

    /// Leases exactly one job from `queue`.
    pub fn lease_one(&self, queue: &'static str) -> LeasedJob {
        let mut leased = self.lease(queue, 1);
        assert_eq!(leased.len(), 1, "expected one job from {queue}");
        leased.remove(0)
    }

    pub fn complete(&self, leased: &LeasedJob) {
        self.engine
            .complete(&leased.job.id, &CompleteRequest::new(leased.token))
            .expect("complete");
    }

    pub fn fail(&self, leased: &LeasedJob, retryable: bool) -> JobState {
        let mut req = FailRequest::new(leased.token, "boom");
        req.retryable = retryable;
        self.engine.fail(&leased.job.id, &req).expect("fail")
    }

    pub fn heartbeat(&self, leased: &LeasedJob) -> HeartbeatResponse {
        self.engine
            .heartbeat(&leased.job.id, &HeartbeatRequest::new(leased.token))
            .expect("heartbeat")
    }

    pub fn job(&self, id: JobId) -> Job {
        self.engine.get(&id).expect("job exists")
    }

    pub fn state(&self, id: JobId) -> JobState {
        self.job(id).state
    }

    /// Every job, oldest first.
    pub fn all_jobs(&self) -> Vec<Job> {
        let mut filter = JobFilter::new()
            .order(belay_proto::api::SortOrder::Asc)
            .limit(1000);
        let mut out = Vec::new();
        loop {
            let page = self.engine.list(&filter).expect("list");
            out.extend(page.items);
            match page.next_cursor {
                Some(cursor) => filter = filter.after(cursor),
                None => return out,
            }
        }
    }

    /// Takes the event type names emitted so far.
    pub fn events(&self) -> Vec<&'static str> {
        let names = self.sink.type_names();
        self.sink.take();
        names
    }

    /// Fails the test if the engine's indexes disagree with its store.
    pub fn assert_consistent(&self) {
        let report = self.engine.verify().expect("verify");
        assert!(
            report.is_consistent(),
            "inconsistent: {:?}",
            report.problems
        );
    }
}

/// A store whose writes can be made to fail, to check that a failed
/// commit leaves the engine unchanged.
#[derive(Debug, Default)]
pub struct FlakyKv {
    inner: MemKv,
    failing: AtomicBool,
    writes: Mutex<u64>,
}

impl FlakyKv {
    pub fn new() -> FlakyKv {
        FlakyKv::default()
    }

    pub fn fail_writes(&self, fail: bool) {
        self.failing.store(fail, Ordering::SeqCst);
    }

    pub fn writes(&self) -> u64 {
        *self.writes.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Kv for FlakyKv {
    fn get(&self, key: &[u8]) -> belay_store::Result<Option<Vec<u8>>> {
        self.inner.get(key)
    }

    fn scan(&self, range: KeyRange) -> belay_store::Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.inner.scan(range)
    }

    fn write(&self, batch: WriteBatch) -> belay_store::Result<u64> {
        if self.failing.load(Ordering::SeqCst) {
            return Err(StoreError::Failed {
                reason: "injected write failure".into(),
            });
        }
        *self.writes.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        self.inner.write(batch)
    }

    fn flush(&self) -> belay_store::Result<()> {
        self.inner.flush()
    }

    fn stats(&self) -> KvStats {
        self.inner.stats()
    }

    fn checkpoint(&self) -> belay_store::Result<CheckpointInfo> {
        self.inner.checkpoint()
    }
}
