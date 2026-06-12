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
