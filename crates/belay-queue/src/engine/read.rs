//! Reads: single jobs, filtered listings, queue and worker reports, and
//! the metrics snapshot.
//!
//! Jobs come back [redacted](belay_proto::Job::redacted): only a lease
//! response carries a lease token, so read access does not let anyone act
//! on another worker's lease.
//!
//! # Listing
//!
//! A listing walks job ids (creation order) from the cursor, in the
//! filter's direction. Criteria the in-memory index knows (queue, state,
//! tenant, creation time) are checked under the engine lock; the
//! candidates are then read from the store without it and checked in full
//! with [`JobFilter::matches`] (kind and tag live only in the document).
//! Each round looks at a bounded number of index entries, so a listing
//! with a rare filter over a large store never holds the lock for long.
//!
//! Pages are stable: the cursor is the last job id returned, and ids never
//! change, so paging continues correctly while jobs are added (they land
//! at the end in ascending order, or before the first page in descending
//! order) or removed.

use std::collections::BTreeSet;
use std::ops::Bound;

use belay_json::PathSegment;
use belay_proto::api::{Cursor, JobFilter, Page, SortOrder, WorkerInfo};
use belay_proto::{Job, JobId, JobState, QueueInfo, QueueName, StateCounts, Violations};

use super::QueueEngine;
use crate::error::{Error, Result};
use crate::meta::Meta;
use crate::records::decode_job;
use crate::state::State;
use crate::stats::{EngineStats, LeaseLatency, QueueStats};

/// Index entries a listing looks at per lock acquisition.
const LIST_SCAN_BUDGET: usize = 4096;

impl QueueEngine {
    /// The job with this id.
    pub fn get(&self, id: &JobId) -> Result<Job> {
        self.load(*id)?
            .map(|job| job.redacted())
            .ok_or(Error::JobNotFound(*id))
    }

    /// The job's current state, from memory. Cheaper than
    /// [`get`](Self::get) for callers that only need the state, such as a
    /// scheduler checking whether the previous run of a schedule finished.
    #[must_use]
    pub fn job_state(&self, id: &JobId) -> Option<JobState> {
        self.lock().st.jobs.get(id).map(|meta| meta.state)
    }

    /// Reads a job from the store, without the engine lock.
    fn load(&self, id: JobId) -> Result<Option<Job>> {
        let key = self.layout.job_key(id);
        match self.kv.get(&key)? {
            Some(bytes) => decode_job(&self.layout, &key, &bytes).map(Some),
            None => Ok(None),
        }
    }

    /// Jobs matching `filter`, one page at a time.
    ///
    /// ```
    /// use std::sync::Arc;
    /// use belay_common::{ManualClock, Timestamp};
    /// use belay_proto::api::{JobFilter, SortOrder};
    /// use belay_proto::source::NoopSink;
    /// use belay_proto::{JobSpec, QueueName};
    /// use belay_queue::{EngineOptions, QueueEngine};
    /// use belay_store::MemKv;
    ///
    /// let clock = ManualClock::new(Timestamp::from_secs(1_700_000_000));
    /// let engine = QueueEngine::open(Arc::new(MemKv::new()), clock.shared(), NoopSink::shared(), EngineOptions::new())?;
    /// for kind in ["a", "b", "c"] {
    ///     engine.enqueue(JobSpec::new("q", kind))?;
    /// }
    /// let filter = JobFilter::new().queue(QueueName::from_static("q")).order(SortOrder::Asc).limit(2);
    /// let first = engine.list(&filter)?;
    /// assert_eq!(first.items.iter().map(|j| j.kind()).collect::<Vec<_>>(), ["a", "b"]);
    /// let rest = engine.list(&filter.after(first.next_cursor.unwrap()))?;
    /// assert_eq!(rest.items[0].kind(), "c");
    /// assert!(rest.next_cursor.is_none());
    /// # Ok::<(), belay_queue::Error>(())
    /// ```
    pub fn list(&self, filter: &JobFilter) -> Result<Page<Job>> {
        let mut position = match &filter.cursor {
            None => None,
            Some(cursor) => Some(cursor.job_id().ok_or_else(|| {
                Error::Invalid(Violations::single(
                    &[PathSegment::Key("cursor".to_owned())],
                    "was not issued by a job listing",
                ))
            })?),
        };
        let limit = filter.effective_limit();
        let want = limit as usize + 1;
        let mut found: Vec<Job> = Vec::new();
        loop {
            let round = {
                let core = self.lock();
                candidates(&core.st, filter, position, want - found.len())
            };
            for id in round.ids {
                // Gone since the round was collected: skip it.
                let Some(job) = self.load(id)? else {
                    continue;
                };
                if filter.matches(&job) {
                    found.push(job);
                    if found.len() == want {
                        break;
                    }
                }
            }
            if found.len() >= want || round.exhausted {
                break;
            }
            position = round.last;
        }
        Ok(Page::paginate(found, limit, |job| {
            Cursor::after_job(job.id)
        }))
    }

    /// Status of one queue.
    pub fn queue_info(&self, queue: &QueueName) -> Result<QueueInfo> {
        let now = self.clock.now();
        let core = self.lock();
        core.st
            .queues
            .get(queue)
            .map(|q| q.info(now))
            .ok_or_else(|| Error::QueueNotFound(queue.clone()))
    }

    /// Status of every queue, by name. A queue exists from its first
    /// enqueue (or pause, or configuration) until it is deleted.
    #[must_use]
    pub fn list_queues(&self) -> Vec<QueueInfo> {
        let now = self.clock.now();
        let core = self.lock();
        core.st.queues.values().map(|q| q.info(now)).collect()
    }

    /// Workers seen recently, by id: their queues, when they were last
    /// heard from, and what they hold and have done.
    #[must_use]
    pub fn workers(&self) -> Vec<WorkerInfo> {
        self.lock().st.workers.list()
    }

    /// A snapshot of counts, counters and latencies for metrics.
    #[must_use]
    pub fn stats(&self) -> EngineStats {
        let now = self.clock.now();
        let core = self.lock();
        let st = &core.st;
        let mut totals = StateCounts::default();
        let queues = st
            .queues
            .values()
            .map(|q| {
                totals += q.counts;
                QueueStats {
                    name: q.name.clone(),
                    paused: q.paused,
                    counts: q.counts,
                    oldest_ready_age: q
                        .oldest_ready()
                        .map(|since| now.saturating_duration_since(since)),
                    throughput: q.throughput.snapshot(now),
                    unique_keys: q.claim_count(),
                    tenants: q.ready.tenants(),
                    rate_tokens: q.bucket.map(|b| b.available(now)),
                }
            })
            .collect();
        let mut counters = st.counters;
        counters.lease_lost += self.lease_lost.load(std::sync::atomic::Ordering::Relaxed);
        EngineStats {
            queues,
            totals,
            counters,
            lease_latency: st.latency.summary(),
            latency_samples_dropped: st.latency.dropped(),
            workers: st.workers.len(),
            waiting_leases: self.waiting.load(std::sync::atomic::Ordering::SeqCst),
        }
    }

    /// Takes the lease latency samples recorded since the last call, for a
    /// metrics exporter to feed into its histogram.
    pub fn drain_lease_latencies(&self) -> Vec<LeaseLatency> {
        self.lock().st.latency.drain()
    }
}

/// One round of a listing: ids passing the index criteria, where the
/// round stopped, and whether it reached the end.
struct Round {
    ids: Vec<JobId>,
    last: Option<JobId>,
    exhausted: bool,
}

/// Collects up to `max` ids after `position` that pass the criteria the
/// index can check, looking at no more than [`LIST_SCAN_BUDGET`] entries.
fn candidates(st: &State, filter: &JobFilter, position: Option<JobId>, max: usize) -> Round {
    let index: Box<dyn Iterator<Item = JobId> + '_> = match &filter.queue {
        Some(queue) => match st.queues.get(queue) {
            Some(q) => walk(&q.jobs, position, filter.order),
            None => {
                return Round {
                    ids: Vec::new(),
                    last: position,
                    exhausted: true,
                }
            }
        },
        None => {
            let start = position.map_or(Bound::Unbounded, Bound::Excluded);
            match (filter.order, start) {
                (SortOrder::Asc, start) => {
                    Box::new(st.jobs.range((start, Bound::Unbounded)).map(|(id, _)| *id))
                }
                (SortOrder::Desc, start) => Box::new(
                    st.jobs
                        .range((Bound::Unbounded, start))
                        .rev()
                        .map(|(id, _)| *id),
                ),
            }
        }
    };
    let mut round = Round {
        ids: Vec::new(),
        last: position,
        exhausted: true,
    };
    for (scanned, id) in index.enumerate() {
        if scanned == LIST_SCAN_BUDGET || round.ids.len() == max {
            round.exhausted = false;
            break;
        }
        round.last = Some(id);
        if st.jobs.get(&id).is_some_and(|meta| prefilter(filter, meta)) {
            round.ids.push(id);
        }
    }
    round
}

/// Ids of a per-queue set after `position` in `order`.
fn walk(
    ids: &BTreeSet<JobId>,
    position: Option<JobId>,
    order: SortOrder,
) -> Box<dyn Iterator<Item = JobId> + '_> {
    let bound = position.map_or(Bound::Unbounded, Bound::Excluded);
    match order {
        SortOrder::Asc => Box::new(ids.range((bound, Bound::Unbounded)).copied()),
        SortOrder::Desc => Box::new(ids.range((Bound::Unbounded, bound)).rev().copied()),
    }
}

/// The part of [`JobFilter::matches`] that the index can answer.
fn prefilter(filter: &JobFilter, meta: &Meta) -> bool {
    (filter.states.is_empty() || filter.states.contains(&meta.state))
        && filter
            .tenant
            .as_ref()
            .map_or(true, |t| meta.tenant.as_ref() == Some(t))
        && filter.created_after.map_or(true, |t| meta.created_at > t)
        && filter.created_before.map_or(true, |t| meta.created_at < t)
}
