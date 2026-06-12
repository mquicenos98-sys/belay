//! `worker-e2e`: jobs through the worker runtime with handlers that do
//! nothing, so the numbers are the runtime's own cost.
//!
//! - `mode=drain`: a backlog is enqueued while the queue is paused, then
//!   the queue is resumed and the clock runs until the worker has
//!   completed every job. Throughput only; per-job latency in a drained
//!   backlog is mostly time spent waiting behind other jobs.
//! - `mode=pickup`: one job at a time is enqueued into an idle worker's
//!   queue; the latency is from `enqueue` until the handler starts, i.e.
//!   the long-poll wake-up, the lease and the dispatch to an executor.
//!
//! `source=embedded` leases in-process from the engine (as `belay dev`
//! does), `source=http` through `belay-client` and `belay-server` over
//! loopback.

use std::fmt;
use std::net::TcpListener;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use belay_client::{Client, ClientConfig};
use belay_core::{Config, Engine};
use belay_proto::{JobId, JobSpec, QueueName, WorkerId};
use belay_server::ServerHandle;
use belay_worker::{handler_fn, Outcome, Registry, Worker, WorkerConfig, WorkerHandle};

use crate::harness::{measure, Params, Plan, Sample, Work};
use crate::session::{CaseResult, Session};
use crate::{Error, Result};

pub const NAME: &str = "worker-e2e";

/// How long a lease call waits for work. Short, because stopping a worker
/// waits for its in-flight lease call; an enqueue wakes the call at once
/// anyway.
const POLL_WAIT: Duration = Duration::from_millis(100);

/// Longest a drain may take before the case is declared failed.
const DRAIN_DEADLINE: Duration = Duration::from_secs(300);

/// Longest a single job may wait to be picked up.
const PICKUP_DEADLINE: Duration = Duration::from_secs(10);
