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

/// Ids of `(time, id)` entries at or before `until`, at most `limit`.
fn up_to(set: &BTreeSet<(Timestamp, JobId)>, until: Timestamp, limit: usize) -> Vec<JobId> {
    set.range((Bound::Unbounded, Bound::Included((until, MAX_ID))))
        .take(limit)
        .map(|(_, id)| *id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_proto::{Job, JobSpec, Lease, LeaseToken, RateLimit, WorkerId};

    fn at(secs: i64) -> Timestamp {
        Timestamp::from_secs(1_000 + secs)
    }

    fn id(n: u128) -> JobId {
        JobId::from_ulid(Ulid::from_u128(n))
    }

    fn queue() -> QueueState {
        QueueState::new(
            QueueName::from_static("q"),
            QueueRecord::default(),
            at(0),
            Duration::from_secs(60),
        )
    }

    fn job(n: u128, spec: JobSpec) -> Job {
        Job::new(id(n), spec, at(0))
    }

    #[test]
    fn every_state_lands_in_exactly_one_index_and_leaves_no_trace() {
        let mut q = queue();
        let mut metas = Vec::new();

        let ready = job(1, JobSpec::new("q", "k"));
        let scheduled = job(2, JobSpec::new("q", "k").delay(Duration::from_secs(5)));
        let mut running = job(3, JobSpec::new("q", "k"));
        running.transition(JobState::Running, at(1)).expect("run");
        running.lease = Some(Lease::new(
            WorkerId::from_static("w"),
            LeaseToken::new(1),
            at(1),
            Duration::from_secs(30),
        ));
        let mut dead = job(4, JobSpec::new("q", "k"));
        dead.transition(JobState::Cancelled, at(2)).expect("cancel");

        for j in [&ready, &scheduled, &running, &dead] {
            let meta = Meta::of(j);
            q.insert(j.id, &meta);
            metas.push((j.id, meta));
        }
        assert_eq!(q.index_sizes(), [1, 1, 1, 1, 1]);
        assert_eq!(q.counts.total(), 4);
        assert_eq!(q.oldest_ready(), Some(at(0)));
        assert_eq!(q.next_timer(), Some(at(5)));
        assert_eq!(q.due_timers(at(4), 10), Vec::<JobId>::new());
        assert_eq!(q.due_timers(at(5), 10), [id(2)]);
        assert_eq!(q.expired_leases(at(30), 10), Vec::<JobId>::new());
        assert_eq!(q.expired_leases(at(31), 10), [id(3)]);

        for (id, meta) in &metas {
            q.remove(*id, meta);
        }
        assert_eq!(q.index_sizes(), [0; 5]);
        assert!(q.jobs.is_empty());
        assert_eq!(q.counts, StateCounts::default());
    }

    #[test]
    fn retention_uses_the_per_state_period() {
        let mut q = queue();
        q.config.retention.succeeded = Duration::from_secs(10);
        q.config.retention.dead = Duration::from_secs(100);
        let mut ok = job(1, JobSpec::new("q", "k"));
        ok.transition(JobState::Running, at(0)).expect("run");
        ok.transition(JobState::Succeeded, at(0)).expect("succeed");
        let mut dead = job(2, JobSpec::new("q", "k"));
        dead.transition(JobState::Running, at(0)).expect("run");
        dead.transition(JobState::Dead, at(0)).expect("die");
        for j in [&ok, &dead] {
            q.insert(j.id, &Meta::of(j));
        }
        assert!(q.retention_due(at(9), 10).is_empty());
        assert_eq!(q.retention_due(at(10), 10), [id(1)]);
        assert_eq!(q.retention_due(at(100), 10), [id(1), id(2)]);
        assert_eq!(q.retention_due(at(100), 1), [id(1)]);
    }

    #[test]
    fn claims_keep_the_expiry_index_exact() {
        let mut q = queue();
        q.set_claim("a", Some(Claim::new(id(1), Some(at(10)))));
        q.set_claim("b", Some(Claim::new(id(2), None)));
        assert_eq!(q.claim_count(), 2);
        assert_eq!(q.expired_claims(at(10), 10), ["a"]);
        // Re-claiming with a later expiry replaces the old index entry.
        q.set_claim("a", Some(Claim::new(id(3), Some(at(20)))));
        assert!(q.expired_claims(at(10), 10).is_empty());
        assert_eq!(q.claim("a").map(|c| c.job), Some(id(3)));
        q.set_claim("a", None);
        assert!(q.expired_claims(at(99), 10).is_empty());
        assert_eq!(q.claim_count(), 1);
    }

    #[test]
    fn capacity_honours_pause_and_concurrency() {
        let mut q = queue();
        assert!(q.has_capacity());
        q.config.max_concurrency = Some(1);
        q.counts.running = 1;
        assert!(!q.has_capacity());
        q.counts.running = 0;
        q.paused = true;
        assert!(!q.has_capacity());
    }

    #[test]
    fn updates_create_reconfigure_and_drop_the_bucket() {
        let mut q = queue();
        assert!(q.bucket.is_none());
        let mut record = q.record();
        record.config.rate_limit = Some(RateLimit::new(1.0, 2));
        q.update(record.clone(), at(0));
        assert_eq!(q.bucket.map(|b| b.limit().burst), Some(2));
        record.config.rate_limit = Some(RateLimit::new(5.0, 5));
        q.update(record.clone(), at(0));
        assert_eq!(q.bucket.map(|b| b.limit().burst), Some(5));
        record.config.rate_limit = None;
        record.paused = true;
        q.update(record, at(0));
        assert!(q.bucket.is_none() && q.paused);
    }

    #[test]
    fn range_helpers_cover_ties_on_time() {
        let mut set = BTreeSet::new();
        set.insert((at(5), id(9)));
        set.insert((at(5), id(1)));
        set.insert((at(6), id(2)));
        assert_eq!(up_to(&set, at(5), 10), [id(1), id(9)]);
        assert_eq!(up_to(&set, at(4), 10), Vec::<JobId>::new());
        assert_eq!(up_to(&set, at(6), 2), [id(1), id(9)]);
    }
}
