//! Durability: what survives a restart or a crash, how the indexes are
//! rebuilt, and what a failed store write leaves behind (nothing).

mod support;

use std::sync::Arc;
use std::time::Duration;

use belay_common::{BackoffPolicy, Rng};
use belay_json::json;
use belay_proto::api::{CompleteRequest, ErrorCode, HeartbeatRequest, LeasedJob};
use belay_proto::{Job, JobId, JobSpec, JobState, QueueConfig, StateCounts, TenantId, UniqueScope};
use belay_queue::{EngineOptions, Error, QueueEngine};
use belay_store::vfs::{Crash, MemFs};
use belay_store::{Kv, LogKv, MemKv, Namespace, Options, SyncPolicy};
use support::*;

/// What must survive: every job as stored, and each queue's persisted
/// settings and counts. Throughput windows and the worker registry are
/// memory-only by design and left out.
#[derive(Debug, PartialEq)]
struct Snapshot {
    jobs: Vec<Job>,
    queues: Vec<(String, bool, QueueConfig, StateCounts, Option<Duration>)>,
    claims: Vec<(String, usize)>,
}

fn snapshot(h: &Harness) -> Snapshot {
    Snapshot {
        jobs: h.all_jobs(),
        queues: h
            .engine
            .list_queues()
            .into_iter()
            .map(|i| {
                (
                    i.name.to_string(),
                    i.paused,
                    i.config,
                    i.counts,
                    i.oldest_ready_age,
                )
            })
            .collect(),
        claims: h
            .engine
            .stats()
            .queues
            .into_iter()
            .map(|s| (s.name.to_string(), s.unique_keys))
            .collect(),
    }
}

/// A deterministic workload: each step is one acknowledged operation,
/// chosen by a seeded generator from what the engine currently holds.
struct Workload {
    rng: Rng,
    leased: Vec<LeasedJob>,
    ids: Vec<JobId>,
}
