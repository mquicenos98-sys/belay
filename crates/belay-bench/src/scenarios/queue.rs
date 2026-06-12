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
