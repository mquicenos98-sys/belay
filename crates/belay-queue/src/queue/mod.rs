//! Per-queue in-memory state: configuration, counts and the ordered
//! indexes the engine schedules from.
//!
//! | index | holds | ordered by | used for |
//! |---|---|---|---|
//! | `ready` | ready jobs | tenant flow, then priority/run_at/id | leasing ([`fair`]) |
//! | `ready_since` | ready jobs | eligibility time | oldest ready age |
//! | `timers` | scheduled and retrying jobs | `run_at` | promotion |
//! | `leases` | running jobs | lease expiry | the lease reaper |
//! | `finished` | terminal jobs, one set per state | `finished_at` | retention |
//! | `claims` / `claim_expiry` | uniqueness keys | key / `until` | dedup, timed release |
//!
//! They are ordered sets rather than binary heaps so that a job leaving an
//! index early (cancelled while scheduled, completed before its lease ran
//! out) is removed in O(log n) instead of lingering as a tombstone; the
//! next due entry is still the first element, so `tick` costs a few
//! lookups per queue when nothing is due.
//!
//! Every job change goes through [`QueueState::remove`] with the job's old
//! [`Meta`] and [`QueueState::insert`] with its new one. There is no
//! per-transition bookkeeping to get wrong: whatever the transition, the
//! indexes end up exactly as they would be if rebuilt from scratch.

pub(crate) mod bucket;

pub(crate) mod fair;

pub(crate) mod throughput;

use std::collections::{BTreeSet, HashMap};
use std::ops::Bound;
use std::time::Duration;

use belay_common::{Timestamp, Ulid};
use belay_proto::{JobId, JobState, QueueConfig, QueueInfo, QueueName, StateCounts};

use self::bucket::TokenBucket;
use self::fair::FairQueue;
use self::throughput::{Metric, ThroughputWindow};
use crate::meta::Meta;
use crate::records::{Claim, QueueRecord};

/// The largest id, to bound range scans over `(time, id)` sets.
const MAX_ID: JobId = JobId::from_ulid(Ulid::MAX);
