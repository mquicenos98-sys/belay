//! belay's job queue engine: durable jobs with leases, retries,
//! uniqueness, per-queue concurrency and rate limits, fair sharing between
//! tenants, and retention, on top of any [`belay_store::Kv`].
//!
//! [`QueueEngine`] is the one type most callers need. It is `Send + Sync`
//! and meant to be shared as `Arc<QueueEngine>`; the HTTP server, the
//! scheduler, the workflow engine and in-process workers all call into the
//! same instance.
//!
//! ```
//! use std::sync::Arc;
//! use std::time::Duration;
//! use belay_common::{ManualClock, Timestamp};
//! use belay_json::json;
//! use belay_proto::api::{CompleteRequest, LeaseRequest};
//! use belay_proto::source::CollectingSink;
//! use belay_proto::{JobSpec, JobState, QueueName, WorkerId};
//! use belay_queue::{EngineOptions, QueueEngine};
//! use belay_store::MemKv;
//!
//! let clock = ManualClock::new(Timestamp::from_secs(1_700_000_000));
//! let events = Arc::new(CollectingSink::new());
//! let engine = QueueEngine::open(
//!     Arc::new(MemKv::new()),
//!     clock.shared(),
//!     events.clone(),
//!     EngineOptions::new(),
//! )?;
//!
//! let id = engine
//!     .enqueue(JobSpec::new("emails", "send_welcome").payload(json!({"user": 42})))?
//!     .id;
//!
//! let worker = WorkerId::from_static("worker-1");
//! let req = LeaseRequest::new(worker, vec![QueueName::from_static("emails")])
//!     .lease_for(Duration::from_secs(60));
//! let leased = engine.lease(&req)?;
//! assert_eq!(leased[0].job.id, id);
//!
//! clock.advance(Duration::from_secs(2));
//! engine.complete(&id, &CompleteRequest::new(leased[0].token))?;
//! assert_eq!(engine.get(&id)?.state, JobState::Succeeded);
//! assert_eq!(
//!     events.type_names(),
//!     ["job.enqueued", "worker.seen", "job.started", "job.succeeded"]
//! );
//! # Ok::<(), belay_queue::Error>(())
//! ```
//!
//! # Job lifecycle
//!
//! Jobs follow [`JobState`](belay_proto::JobState)'s machine: they enter as
//! `scheduled` (future `run_at`) or `ready`; a lease makes them `running`;
//! a worker then completes them (`succeeded`) or fails them (`retrying`
//! with a backoff delay, or `dead` when attempts are exhausted or the
//! error is not retryable). A lease that runs out counts as a failed
//! attempt. `cancel`, `retry` and `promote` let operators intervene; the
//! [`QueueEngine`] method docs give the exact rules.
//!
//! Every mutation by a worker presents the lease token it was given; a
//! stale token (the lease expired, the job moved on) is refused with
//! [`Error::LeaseLost`] so that a slow worker cannot overwrite the result
//! of the one that took over.
//!
//! # Persistence
//!
//! Each state change is staged, written to the store as one atomic
//! [`WriteBatch`](belay_store::WriteBatch), and only then applied to
//! memory: if the write fails, the engine is exactly as it was. Jobs are
//! JSON documents keyed by id, queues and uniqueness claims have records of
//! their own, and all in-memory indexes are rebuilt from those when the
//! engine opens. Nothing else is stored, so the store and the indexes
//! cannot drift apart. With a durable store and `SyncPolicy::Always`,
//! every acknowledged operation survives a crash.
//!
//! # Time
//!
//! The engine reads an injected [`Clock`](belay_common::Clock) and starts
//! no threads. Promotion of due jobs, lease expiry, retention and worker
//! liveness happen in [`QueueEngine::tick`], which the embedding process
//! calls periodically. Leasing also promotes the due jobs of the queues it
//! looks at, so a job whose time has come is leasable as soon as a worker
//! asks, whether or not `tick` has run since. Under a `ManualClock`
//! everything is deterministic, which is how the tests drive it.
//!
//! # Limitations
//!
//! - Store writes happen under the engine lock (see [`QueueEngine`] for
//!   why), so under `SyncPolicy::Always` commit throughput is bounded by
//!   the device's sync latency. Batch enqueues and multi-job leases
//!   amortize it.
//! - Opening scans every job; start-up time grows with the number of
//!   retained jobs.
//! - Leases are persisted with their expiry. If the process is down for
//!   longer than a lease, the lease runs out at the next tick even if the
//!   worker is still going; the worker then gets `lease_lost` and the job
//!   runs again.
//! - Fair sharing is per queue; there is no global tenant quota across
//!   queues.

#![forbid(unsafe_code)]
#![warn(missing_docs, missing_debug_implementations)]

mod engine;
mod error;
mod meta;
mod options;
mod queue;
mod records;
mod signal;
mod state;
mod stats;
mod txn;
mod workers;

pub use engine::{QueueEngine, TickReport, VerifyReport};
pub use error::{Error, Result};
pub use options::{EngineOptions, ParseQueueOrderError, QueueOrder};
pub use stats::{Counters, EngineStats, LatencySummary, LeaseLatency, QueueStats};
