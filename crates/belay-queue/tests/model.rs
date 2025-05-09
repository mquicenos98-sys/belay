//! Randomized model-based test: thousands of seeded random operations are
//! applied both to the engine and to a deliberately simple reference
//! model, and the two must agree after every step, across restarts and
//! simulated power losses.
//!
//! The model keeps jobs in a map and answers every question by brute
//! force (sorting all ready jobs to find the next lease, scanning all
//! claims), so it is easy to trust. Jobs carry no tenant and queues no
//! limits, which makes the engine's choices fully predictable: every
//! lease must hand out exactly the best ready jobs of each queue.

mod support;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Duration;

use belay_common::{BackoffPolicy, Rng, Timestamp};
use belay_proto::api::{
    CompleteRequest, EnqueueStatus, FailRequest, HeartbeatRequest, LeaseRequest,
};
use belay_proto::{JobId, JobSpec, JobState, LeaseToken, OnConflict, Unique, UniqueScope};
use belay_queue::{EngineOptions, Error};
use belay_store::vfs::{Crash, MemFs};
use support::*;

const QUEUES: [&str; 3] = ["a", "b", "c"];

const LEASE: Duration = Duration::from_secs(30);

const BACKOFF: Duration = Duration::from_secs(5);
