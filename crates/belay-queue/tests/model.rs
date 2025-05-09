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

#[derive(Clone, Debug)]
struct MJob {
    queue: &'static str,
    priority: i16,
    run_at: Timestamp,
    state: JobState,
    attempt: u32,
    max_attempts: u32,
    token: Option<LeaseToken>,
    expires: Timestamp,
    heartbeat: Timestamp,
    cancel_requested: bool,
    unique: Option<String>,
}

#[derive(Debug, Default)]
struct Model {
    jobs: BTreeMap<JobId, MJob>,
    /// (queue, key) -> holder, while the holder is unfinished.
    claims: HashMap<(&'static str, String), JobId>,
    paused: BTreeSet<&'static str>,
}

impl Model {
    fn release_claim(&mut self, id: JobId) {
        self.claims.retain(|_, holder| *holder != id);
    }

    fn finish(&mut self, id: JobId, state: JobState) {
        let job = self.jobs.get_mut(&id).expect("model job");
        job.state = state;
        job.token = None;
        job.cancel_requested = false;
        self.release_claim(id);
    }

    /// Scheduled and retrying jobs of `queues` whose time has come.
    fn promote(&mut self, queues: &[&'static str], now: Timestamp) -> u64 {
        let mut n = 0;
        for job in self.jobs.values_mut() {
            if queues.contains(&job.queue) && job.state.is_waiting() && job.run_at <= now {
                job.state = JobState::Ready;
                n += 1;
            }
        }
        n
    }

    /// Ready jobs of `queue` in the order the engine must hand them out.
    fn ready(&self, queue: &str) -> Vec<JobId> {
        let mut ready: Vec<(i16, Timestamp, JobId)> = self
            .jobs
            .iter()
            .filter(|(_, j)| j.queue == queue && j.state == JobState::Ready)
            .map(|(id, j)| (-j.priority, j.run_at, *id))
            .collect();
        ready.sort();
        ready.into_iter().map(|(_, _, id)| id).collect()
    }

    /// A failed attempt of a running job, as `fail` or lease expiry does it.
    fn fail(&mut self, id: JobId, retryable: bool, now: Timestamp) -> JobState {
        let job = self.jobs.get(&id).expect("model job");
        let next = if job.cancel_requested {
            JobState::Cancelled
        } else if retryable && job.attempt < job.max_attempts {
            JobState::Retrying
        } else {
            JobState::Dead
        };
        if next == JobState::Retrying {
            let job = self.jobs.get_mut(&id).expect("model job");
            job.state = JobState::Retrying;
            job.run_at = now + BACKOFF;
            job.token = None;
            job.cancel_requested = false;
        } else {
            self.finish(id, next);
        }
        next
    }

    fn holds_lease(&self, id: JobId, token: LeaseToken, now: Timestamp) -> bool {
        self.jobs.get(&id).is_some_and(|j| {
            j.state == JobState::Running && j.token == Some(token) && now < j.expires
        })
    }
}
