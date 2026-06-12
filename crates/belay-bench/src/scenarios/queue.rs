//! `queue-embedded` and `queue-http`: the job lifecycle operations, called
//! in-process on a [`QueueEngine`] or over loopback HTTP through
//! `belay-server` and `belay-client`.
//!
//! | op | one call is |
//! |---|---|
//! | `enqueue` | one `enqueue` |
//! | `enqueue-batch` | one `enqueue_batch` of 100 jobs (100 items) |
//! | `lease-complete` | leasing one job from a pre-filled backlog and completing it |
//! | `cycle` | enqueue, lease and complete one job |
//!
//! All threads (or HTTP clients) share one queue, so the numbers include
//! contention on the engine's lock. Every state change is one store
//! write: under `sync=always` each costs a sync, which the engine does
//! not group across callers (see `store-write` for what a sync costs).

use std::fmt;
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use belay_client::{Client, ClientConfig};
use belay_common::fs::TempDir;
use belay_common::system_clock;
use belay_core::{Config, Engine};
use belay_json::json;
use belay_proto::api::{CompleteRequest, LeaseRequest, LeasedJob};
use belay_proto::limits::MAX_ENQUEUE_BATCH;
use belay_proto::source::NoopSink;
use belay_proto::{JobSpec, QueueName, WorkerId};
use belay_queue::{EngineOptions, QueueEngine};
use belay_store::{Kv, LogKv, MemKv, Options, SyncPolicy};

use crate::harness::{measure_parallel, Params, Plan, Work};
use crate::session::{CaseResult, Session};
use crate::{Error, Result};

pub const EMBEDDED: &str = "queue-embedded";
pub const HTTP: &str = "queue-http";

/// Jobs per `enqueue-batch` call.
const BATCH: u64 = 100;

/// Long enough that no lease expires during a case.
const LEASE_FOR: Duration = Duration::from_secs(600);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Enqueue,
    EnqueueBatch,
    LeaseComplete,
    Cycle,
}

impl Op {
    const ALL: [Op; 4] = [Op::Enqueue, Op::EnqueueBatch, Op::LeaseComplete, Op::Cycle];

    fn items(self) -> u64 {
        if self == Op::EnqueueBatch {
            BATCH
        } else {
            1
        }
    }

    /// Calls for a case, given the job count a case should process.
    fn iterations(self, jobs: u64, smoke: bool) -> u64 {
        let floor = if smoke { 1 } else { 10 };
        (jobs / self.items()).max(floor)
    }
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Op::Enqueue => "enqueue",
            Op::EnqueueBatch => "enqueue-batch",
            Op::LeaseComplete => "lease-complete",
            Op::Cycle => "cycle",
        })
    }
}

/// The calls the operations need, implemented by the engine and by the
/// HTTP client so both scenarios run exactly the same loop.
trait QueueApi {
    fn enqueue(&self, spec: JobSpec) -> Result<()>;
    fn enqueue_batch(&self, specs: Vec<JobSpec>) -> Result<()>;
    fn lease(&self, req: &LeaseRequest) -> Result<Vec<LeasedJob>>;
    fn complete(&self, job: &LeasedJob) -> Result<()>;
}

impl QueueApi for QueueEngine {
    fn enqueue(&self, spec: JobSpec) -> Result<()> {
        QueueEngine::enqueue(self, spec)?;
        Ok(())
    }
    fn enqueue_batch(&self, specs: Vec<JobSpec>) -> Result<()> {
        QueueEngine::enqueue_batch(self, specs)?;
        Ok(())
    }
    fn lease(&self, req: &LeaseRequest) -> Result<Vec<LeasedJob>> {
        Ok(QueueEngine::lease(self, req)?)
    }
    fn complete(&self, job: &LeasedJob) -> Result<()> {
        QueueEngine::complete(self, &job.job.id, &CompleteRequest::new(job.token))?;
        Ok(())
    }
}

impl QueueApi for Client {
    fn enqueue(&self, spec: JobSpec) -> Result<()> {
        self.jobs().enqueue(&spec)?;
        Ok(())
    }
    fn enqueue_batch(&self, specs: Vec<JobSpec>) -> Result<()> {
        self.jobs().enqueue_batch(&specs)?;
        Ok(())
    }
    fn lease(&self, req: &LeaseRequest) -> Result<Vec<LeasedJob>> {
        Ok(self.jobs().lease(req)?)
    }
    fn complete(&self, job: &LeasedJob) -> Result<()> {
        self.jobs()
            .complete(job.job.id, &CompleteRequest::new(job.token))?;
        Ok(())
    }
}

/// A small job, like most real ones: a kind and an id in the payload.
fn spec(queue: &QueueName, seq: u64) -> JobSpec {
    JobSpec::new(queue.clone(), "bench.noop").payload(json!({"seq": seq, "source": "belay-bench"}))
}

fn lease_request(thread: usize, queue: &QueueName) -> LeaseRequest {
    let worker = WorkerId::new(format!("bench-{thread}")).expect("bench-<n> is a valid worker id");
    LeaseRequest::new(worker, vec![queue.clone()]).lease_for(LEASE_FOR)
}

/// One call of `op`; `seq` makes batch payloads distinct.
fn call(
    api: &impl QueueApi,
    op: Op,
    queue: &QueueName,
    lease: &LeaseRequest,
    seq: u64,
) -> Result<Work> {
    match op {
        Op::Enqueue => api.enqueue(spec(queue, seq))?,
        Op::EnqueueBatch => {
            let first = seq * BATCH;
            api.enqueue_batch((first..first + BATCH).map(|n| spec(queue, n)).collect())?;
        }
        Op::LeaseComplete => lease_and_complete(api, lease)?,
        Op::Cycle => {
            api.enqueue(spec(queue, seq))?;
            lease_and_complete(api, lease)?;
        }
    }
    Ok(Work::items(op.items()))
}

fn lease_and_complete(api: &impl QueueApi, lease: &LeaseRequest) -> Result<()> {
    let leased = api.lease(lease)?;
    let job = leased
        .first()
        .ok_or_else(|| Error::Invalid("a lease found no ready job: the backlog ran dry".into()))?;
    api.complete(job)
}

/// Enqueues `jobs` jobs in the largest batches the API accepts; setup for
/// `lease-complete`, not measured.
fn fill_backlog(api: &impl QueueApi, queue: &QueueName, jobs: u64) -> Result<()> {
    let mut next = 0;
    while next < jobs {
        let end = (next + MAX_ENQUEUE_BATCH as u64).min(jobs);
        api.enqueue_batch((next..end).map(|n| spec(queue, n)).collect())?;
        next = end;
    }
    Ok(())
}

/// Where the embedded engine keeps its jobs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    Mem,
    Log(SyncPolicy),
}

pub fn run_embedded(s: &mut Session<'_>) -> Result<()> {
    let backends = [
        Backend::Mem,
        Backend::Log(SyncPolicy::Interval(Duration::from_millis(10))),
        Backend::Log(SyncPolicy::Always),
    ];
    for backend in backends {
        for op in Op::ALL {
            for threads in s.thread_counts() {
                embedded_case(s, backend, op, threads)?;
            }
        }
    }
    Ok(())
}

fn embedded_case(s: &mut Session<'_>, backend: Backend, op: Op, threads: usize) -> Result<()> {
    let mut params = Params::new();
    params = match backend {
        Backend::Mem => params.with("store", "mem"),
        Backend::Log(policy) => params.with("store", "log").with("sync", policy),
    };
    let params = params.with("op", op).with("threads", threads);
    if !s.wants(EMBEDDED, &params) {
        return Ok(());
    }
    let jobs = match backend {
        Backend::Mem => s.pick(100_000, 10_000, 3),
        Backend::Log(SyncPolicy::Always) => s.pick(2_000, 200, 3),
        Backend::Log(_) => s.pick(50_000, 5_000, 3),
    };
    let plan = Plan::new(op.iterations(jobs, s.pick(false, false, true)));

    // Declared before the engine, so dropped after it: the store closes
    // before its directory is removed (Windows cannot delete open files).
    let dir: Option<TempDir> = match backend {
        Backend::Mem => None,
        Backend::Log(_) => Some(s.temp_dir(EMBEDDED)?),
    };
    let kv: Arc<dyn Kv> = match (backend, &dir) {
        (Backend::Log(policy), Some(dir)) => {
            Arc::new(LogKv::open(dir.path(), Options::new().sync(policy))?)
        }
        _ => Arc::new(MemKv::new()),
    };
    let engine = QueueEngine::open(
        kv,
        system_clock(),
        NoopSink::shared(),
        EngineOptions::new().seed(s.seed()),
    )?;
    let queue = QueueName::from_static("bench");
    if op == Op::LeaseComplete {
        fill_backlog(&engine, &queue, plan.total())?;
    }
    let (engine_ref, queue_ref) = (&engine, &queue);
    let sample = measure_parallel(plan, threads, |t| {
        let lease = lease_request(t, queue_ref);
        Ok(move |seq| call(engine_ref, op, queue_ref, &lease, seq))
    })?;
    engine.shutdown();
    s.record(CaseResult::new(EMBEDDED, params, sample));
    Ok(())
}

pub fn run_http(s: &mut Session<'_>) -> Result<()> {
    let mut cases = Vec::new();
    for op in Op::ALL {
        for clients in s.thread_counts() {
            let params = Params::new().with("op", op).with("clients", clients);
            if s.wants(HTTP, &params) {
                cases.push((op, clients, params));
            }
        }
    }
    if cases.is_empty() {
        return Ok(());
    }
    let engine = Arc::new(
        Engine::builder()
            .config(Config::dev())
            .seed(s.seed())
            .start()?,
    );
    let listener =
        TcpListener::bind("127.0.0.1:0").map_err(|e| Error::io("binding a loopback port", e))?;
    let server = belay_server::serve_on(engine, listener)?;
    let url = format!("http://{}", server.local_addr());
    let outcome = http_cases(s, &url, cases);
    server.shutdown_and_join();
    outcome
}

fn http_cases(s: &mut Session<'_>, url: &str, cases: Vec<(Op, usize, Params)>) -> Result<()> {
    let jobs = s.pick(20_000, 2_000, 3);
    let smoke = s.pick(false, false, true);
    let client = || Client::new(ClientConfig::new(url).timeout(Duration::from_secs(30)));
    let setup = client()?;
    for (index, (op, clients, params)) in cases.into_iter().enumerate() {
        // A queue per case, so one case's leftovers never feed another.
        let queue = QueueName::new(format!("bench-http-{index}"))
            .expect("bench-http-<n> is a valid queue name");
        let plan = Plan::new(op.iterations(jobs, smoke));
        if op == Op::LeaseComplete {
            fill_backlog(&setup, &queue, plan.total())?;
        }
        let queue_ref = &queue;
        let sample = measure_parallel(plan, clients, |t| {
            let (client, lease) = (client()?, lease_request(t, queue_ref));
            Ok(move |seq| call(&client, op, queue_ref, &lease, seq))
        })?;
        s.record(CaseResult::new(HTTP, params, sample));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_common::ManualClock;
    use belay_common::Timestamp;

    fn engine() -> QueueEngine {
        // Opening logs at info level; keep the test output readable.
        belay_common::log::set_max_level(belay_common::log::LevelFilter::Warn);
        let clock = ManualClock::new(Timestamp::from_secs(1_700_000_000));
        QueueEngine::open(
            Arc::new(MemKv::new()),
            clock.shared(),
            NoopSink::shared(),
            EngineOptions::new().seed(1),
        )
        .unwrap()
    }

    #[test]
    fn each_op_processes_the_jobs_it_claims() {
        let engine = engine();
        let queue = QueueName::from_static("q");
        let lease = lease_request(0, &queue);
        assert_eq!(
            call(&engine, Op::Enqueue, &queue, &lease, 0).unwrap(),
            Work::ONE
        );
        assert_eq!(
            call(&engine, Op::EnqueueBatch, &queue, &lease, 1).unwrap(),
            Work::items(100)
        );
        assert_eq!(engine.queue_info(&queue).unwrap().counts.ready, 101);
        call(&engine, Op::Cycle, &queue, &lease, 2).unwrap();
        call(&engine, Op::LeaseComplete, &queue, &lease, 3).unwrap();
        let counts = engine.queue_info(&queue).unwrap().counts;
        assert_eq!((counts.ready, counts.succeeded), (100, 2));
    }

    #[test]
    fn an_empty_backlog_is_reported_not_ignored() {
        let engine = engine();
        let queue = QueueName::from_static("q");
        let err = call(
            &engine,
            Op::LeaseComplete,
            &queue,
            &lease_request(0, &queue),
            0,
        )
        .unwrap_err();
        assert!(err.to_string().contains("backlog ran dry"), "{err}");
    }

    #[test]
    fn backlogs_are_filled_in_api_sized_batches() {
        let engine = engine();
        let queue = QueueName::from_static("q");
        fill_backlog(&engine, &queue, 2_345).unwrap();
        assert_eq!(engine.queue_info(&queue).unwrap().counts.ready, 2_345);
    }

    #[test]
    fn batch_ops_need_fewer_calls() {
        assert_eq!(Op::Enqueue.iterations(5_000, false), 5_000);
        assert_eq!(Op::EnqueueBatch.iterations(5_000, false), 50);
        assert_eq!(Op::EnqueueBatch.iterations(200, false), 10);
        assert_eq!(Op::EnqueueBatch.iterations(3, true), 1);
    }
}
