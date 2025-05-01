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

/// Terminal states in the order of the `finished` array.
const TERMINAL: [JobState; 3] = [JobState::Succeeded, JobState::Dead, JobState::Cancelled];

fn terminal_slot(state: JobState) -> Option<usize> {
    TERMINAL.iter().position(|s| *s == state)
}

#[derive(Debug)]
pub(crate) struct QueueState {
    pub(crate) name: QueueName,
    pub(crate) config: QueueConfig,
    pub(crate) paused: bool,
    pub(crate) counts: StateCounts,
    /// Every job in the queue, by id: listing and purging.
    pub(crate) jobs: BTreeSet<JobId>,
    pub(crate) ready: FairQueue,
    ready_since: BTreeSet<(Timestamp, JobId)>,
    timers: BTreeSet<(Timestamp, JobId)>,
    leases: BTreeSet<(Timestamp, JobId)>,
    finished: [BTreeSet<(Timestamp, JobId)>; 3],
    claims: HashMap<String, Claim>,
    claim_expiry: BTreeSet<(Timestamp, String)>,
    pub(crate) bucket: Option<TokenBucket>,
    pub(crate) throughput: ThroughputWindow,
}

impl QueueState {
    pub(crate) fn new(
        name: QueueName,
        record: QueueRecord,
        now: Timestamp,
        window: Duration,
    ) -> QueueState {
        QueueState {
            name,
            bucket: record.config.rate_limit.map(|l| TokenBucket::new(l, now)),
            config: record.config,
            paused: record.paused,
            counts: StateCounts::default(),
            jobs: BTreeSet::new(),
            ready: FairQueue::default(),
            ready_since: BTreeSet::new(),
            timers: BTreeSet::new(),
            leases: BTreeSet::new(),
            finished: Default::default(),
            claims: HashMap::new(),
            claim_expiry: BTreeSet::new(),
            throughput: ThroughputWindow::new(window),
        }
    }

    /// The persisted part of this queue's state.
    pub(crate) fn record(&self) -> QueueRecord {
        QueueRecord {
            config: self.config.clone(),
            paused: self.paused,
        }
    }

    /// Applies a new configuration and pause flag. The token bucket keeps
    /// its tokens across rate changes (see [`TokenBucket::reconfigure`]).
    pub(crate) fn update(&mut self, record: QueueRecord, now: Timestamp) {
        self.bucket = match (self.bucket.take(), record.config.rate_limit) {
            (Some(mut bucket), Some(limit)) => {
                bucket.reconfigure(limit, now);
                Some(bucket)
            }
            (None, Some(limit)) => Some(TokenBucket::new(limit, now)),
            (_, None) => None,
        };
        self.config = record.config;
        self.paused = record.paused;
    }

    /// Indexes a job under its current meta.
    pub(crate) fn insert(&mut self, id: JobId, meta: &Meta) {
        self.jobs.insert(id);
        self.counts.increment(meta.state);
        match meta.state {
            JobState::Ready => {
                self.ready.insert(&meta.tenant, meta.ready_key(id));
                self.ready_since.insert((meta.run_at, id));
            }
            JobState::Scheduled | JobState::Retrying => {
                self.timers.insert((meta.run_at, id));
            }
            JobState::Running => {
                self.leases.insert((meta.lease_deadline(), id));
            }
            terminal => {
                if let Some(slot) = terminal_slot(terminal) {
                    self.finished[slot].insert((meta.finished_or_created(), id));
                }
            }
        }
    }

    /// Removes a job indexed under `meta`, which must be the meta it was
    /// inserted with.
    pub(crate) fn remove(&mut self, id: JobId, meta: &Meta) {
        self.jobs.remove(&id);
        self.counts.decrement(meta.state);
        match meta.state {
            JobState::Ready => {
                self.ready.remove(&meta.tenant, &meta.ready_key(id));
                self.ready_since.remove(&(meta.run_at, id));
            }
            JobState::Scheduled | JobState::Retrying => {
                self.timers.remove(&(meta.run_at, id));
            }
            JobState::Running => {
                self.leases.remove(&(meta.lease_deadline(), id));
            }
            terminal => {
                if let Some(slot) = terminal_slot(terminal) {
                    self.finished[slot].remove(&(meta.finished_or_created(), id));
                }
            }
        }
    }

    /// The claim on `key`, if any (expired timed claims included; callers
    /// check [`Claim::expired`]).
    pub(crate) fn claim(&self, key: &str) -> Option<&Claim> {
        self.claims.get(key)
    }

    /// Sets or clears the claim on `key`, keeping the expiry index exact.
    pub(crate) fn set_claim(&mut self, key: &str, claim: Option<Claim>) {
        if let Some(old) = self.claims.remove(key) {
            if let Some(until) = old.until {
                self.claim_expiry.remove(&(until, key.to_owned()));
            }
        }
        if let Some(claim) = claim {
            if let Some(until) = claim.until {
                self.claim_expiry.insert((until, key.to_owned()));
            }
            self.claims.insert(key.to_owned(), claim);
        }
    }

    pub(crate) fn claim_count(&self) -> usize {
        self.claims.len()
    }

    /// Scheduled and retrying jobs due at `now`, earliest first.
    pub(crate) fn due_timers(&self, now: Timestamp, limit: usize) -> Vec<JobId> {
        up_to(&self.timers, now, limit)
    }

    /// When the earliest timer fires.
    pub(crate) fn next_timer(&self) -> Option<Timestamp> {
        self.timers.first().map(|(at, _)| *at)
    }

    /// Running jobs whose lease has run out at `now` (a lease is valid
    /// strictly before its expiry instant).
    pub(crate) fn expired_leases(&self, now: Timestamp, limit: usize) -> Vec<JobId> {
        up_to(&self.leases, now, limit)
    }

    /// Terminal jobs whose retention has passed at `now`, oldest first per
    /// state.
    pub(crate) fn retention_due(&self, now: Timestamp, limit: usize) -> Vec<JobId> {
        let mut due = Vec::new();
        for (slot, state) in TERMINAL.iter().enumerate() {
            let Some(keep) = self.config.retention.for_state(*state) else {
                continue;
            };
            let Some(cutoff) = now.checked_sub(keep) else {
                continue;
            };
            due.extend(up_to(&self.finished[slot], cutoff, limit - due.len()));
            if due.len() >= limit {
                break;
            }
        }
        due
    }

    /// Timed claims that have run out at `now`.
    pub(crate) fn expired_claims(&self, now: Timestamp, limit: usize) -> Vec<String> {
        self.claim_expiry
            .iter()
            .take_while(|(until, _)| *until <= now)
            .take(limit)
            .map(|(_, key)| key.clone())
            .collect()
    }

    /// When the longest-waiting ready job became eligible.
    pub(crate) fn oldest_ready(&self) -> Option<Timestamp> {
        self.ready_since.first().map(|(at, _)| *at)
    }

    /// Names of the indexes in which `self` and `other` differ, for
    /// consistency checks against a copy rebuilt from the store. The order
    /// of the tenant ring is not compared: it reflects the history of
    /// turns, not the jobs.
    pub(crate) fn differences(&self, other: &QueueState) -> Vec<&'static str> {
        let mut diff = Vec::new();
        let mut check = |name: &'static str, same: bool| {
            if !same {
                diff.push(name);
            }
        };
        check("config", self.config == other.config);
        check("paused", self.paused == other.paused);
        check("counts", self.counts == other.counts);
        check("jobs", self.jobs == other.jobs);
        let ready: BTreeSet<_> = self.ready.iter().collect();
        let other_ready: BTreeSet<_> = other.ready.iter().collect();
        check(
            "ready",
            self.ready.len() == other.ready.len() && ready == other_ready,
        );
        check(
            "ready tenants",
            self.ready.tenants() == other.ready.tenants(),
        );
        check("ready_since", self.ready_since == other.ready_since);
        check("timers", self.timers == other.timers);
        check("leases", self.leases == other.leases);
        check("finished", self.finished == other.finished);
        check("claims", self.claims == other.claims);
        check("claim_expiry", self.claim_expiry == other.claim_expiry);
        diff
    }

    pub(crate) fn record_metric(&mut self, now: Timestamp, metric: Metric) {
        self.throughput.record(now, metric);
    }

    /// Whether the queue can hand out another job right now, capacity and
    /// pause-wise (tokens aside).
    pub(crate) fn has_capacity(&self) -> bool {
        !self.paused
            && self
                .config
                .max_concurrency
                .map_or(true, |max| self.counts.running < u64::from(max))
    }

    pub(crate) fn info(&self, now: Timestamp) -> QueueInfo {
        QueueInfo {
            name: self.name.clone(),
            paused: self.paused,
            config: self.config.clone(),
            counts: self.counts,
            oldest_ready_age: self
                .oldest_ready()
                .map(|since| now.saturating_duration_since(since)),
            throughput: self.throughput.snapshot(now),
        }
    }

    #[cfg(test)]
    pub(crate) fn index_sizes(&self) -> [usize; 5] {
        [
            self.ready.len(),
            self.ready_since.len(),
            self.timers.len(),
            self.leases.len(),
            self.finished.iter().map(BTreeSet::len).sum(),
        ]
    }
}
