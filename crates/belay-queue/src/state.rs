//! The engine's committed in-memory state and how a transaction is applied
//! to it.
//!
//! [`State`] only ever changes in [`State::apply`], which runs after the
//! transaction's batch has been written to the store. Applying cannot fail:
//! everything that could go wrong (missing jobs, refused transitions,
//! uniqueness conflicts, the write itself) has been dealt with while
//! staging, so memory and store move together or not at all.

use std::collections::BTreeMap;
use std::time::Duration;

use belay_common::id::UlidGenerator;
use belay_common::{Rng, Timestamp, Ulid};
use belay_proto::event::EventKind;
use belay_proto::{Job, JobId, JobState, QueueName};

use crate::error::{Error, Result};
use crate::meta::Meta;
use crate::queue::QueueState;
use crate::records::QueueRecord;
use crate::stats::{Counters, LatencyLog};
use crate::txn::{Effects, Txn};
use crate::workers::Workers;

/// Everything behind the engine lock.
#[derive(Debug)]
pub(crate) struct Core {
    pub(crate) st: State,
    pub(crate) gen: Gen,
}

/// Committed indexes and bookkeeping.
#[derive(Debug)]
pub(crate) struct State {
    /// Every job, by id: lookups by id and unfiltered listings.
    pub(crate) jobs: BTreeMap<JobId, Meta>,
    pub(crate) queues: BTreeMap<QueueName, QueueState>,
    pub(crate) workers: Workers,
    pub(crate) counters: Counters,
    pub(crate) latency: LatencyLog,
    /// Where the next round-robin lease request starts.
    pub(crate) round_robin: usize,
    /// Throughput window for queues created from now on.
    window: Duration,
}

/// Sources of randomness and ids. Kept apart from [`State`] so a staging
/// context can draw from them while it borrows the committed state.
pub(crate) struct Gen {
    pub(crate) rng: Rng,
    ids: UlidGenerator,
    /// The newest id handed out or found in the store; new ids go above it.
    floor: Option<Ulid>,
}

impl std::fmt::Debug for Gen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gen")
            .field("floor", &self.floor)
            .finish_non_exhaustive()
    }
}

impl Gen {
    /// Randomness from `seed` (or entropy); ids above `newest`, the newest
    /// job id already stored.
    pub(crate) fn new(seed: Option<u64>, newest: Option<JobId>) -> Gen {
        let mut rng = seed.map_or_else(Rng::from_entropy, Rng::seed_from_u64);
        let ids = UlidGenerator::with_rng(rng.fork());
        Gen {
            rng,
            ids,
            floor: newest.map(JobId::ulid),
        }
    }

    /// A fresh job id for time `now`. Ids come from the engine's clock, so
    /// under a manual clock they carry the test's time.
    ///
    /// The ULID generator is only monotonic within one process. After a
    /// restart in the same millisecond, or after the clock stepped back,
    /// it could produce an id at or below one already stored, which would
    /// overwrite that job; ids are therefore kept above the newest one the
    /// engine has seen, stored or minted.
    pub(crate) fn job_id(&mut self, now: Timestamp) -> Result<JobId> {
        let exhausted =
            |e: &dyn std::fmt::Display| Error::Internal(format!("cannot mint a job id: {e}"));
        let mut id = self.ids.generate(now).map_err(|e| exhausted(&e))?;
        if let Some(floor) = self.floor {
            if id <= floor {
                id = floor
                    .increment()
                    .ok_or_else(|| exhausted(&"id space above the newest job is used up"))?;
            }
        }
        self.floor = Some(id);
        Ok(JobId::from_ulid(id))
    }
}

impl State {
    pub(crate) fn new(window: Duration, latency_samples: usize) -> State {
        State {
            jobs: BTreeMap::new(),
            queues: BTreeMap::new(),
            workers: Workers::default(),
            counters: Counters::default(),
            latency: LatencyLog::new(latency_samples),
            round_robin: 0,
            window,
        }
    }

    /// Creates `name` with `record`, or updates it if it exists.
    pub(crate) fn upsert_queue(&mut self, name: &QueueName, record: QueueRecord, now: Timestamp) {
        match self.queues.get_mut(name) {
            Some(queue) => queue.update(record, now),
            None => {
                let queue = QueueState::new(name.clone(), record, now, self.window);
                self.queues.insert(name.clone(), queue);
            }
        }
    }

    /// Replaces the indexed version of job `id` with `job`, or drops it
    /// when `job` is `None`. Keeps the worker registry's `active_jobs` in
    /// step with running leases.
    pub(crate) fn apply_job(&mut self, id: JobId, job: Option<&Job>, now: Timestamp) {
        if let Some(old) = self.jobs.remove(&id) {
            if let Some(queue) = self.queues.get_mut(&old.queue) {
                queue.remove(id, &old);
            }
            if old.state == JobState::Running {
                if let Some(lease) = &old.lease {
                    self.workers.release(&lease.worker);
                }
            }
        }
        let Some(job) = job else {
            return;
        };
        let meta = Meta::of(job);
        if !self.queues.contains_key(&meta.queue) {
            // Staging creates queues before putting jobs in them; this only
            // covers records written by something else.
            self.upsert_queue(&meta.queue, QueueRecord::default(), now);
        }
        if let Some(queue) = self.queues.get_mut(&meta.queue) {
            queue.insert(id, &meta);
        }
        if meta.state == JobState::Running {
            if let Some(lease) = &meta.lease {
                self.workers.acquire(&lease.worker, now);
            }
        }
        self.jobs.insert(id, meta);
    }

    /// Applies a transaction whose batch has been persisted. Returns the
    /// events to emit and the effects to run once the lock is released.
    pub(crate) fn apply(&mut self, tx: Txn) -> (Vec<EventKind>, Effects) {
        let now = tx.now;
        let mut events = tx.events;
        for (name, record) in &tx.queues {
            if let Some(record) = record {
                self.upsert_queue(name, record.clone(), now);
            }
        }
        for plan in tx.plans {
            if let Some(queue) = self.queues.get_mut(&plan.queue) {
                queue.ready.install(plan.cursor);
                queue.bucket = plan.bucket;
            }
        }
        if let Some(next) = tx.round_robin {
            self.round_robin = next;
        }
        for (id, job) in &tx.jobs {
            self.apply_job(*id, job.as_ref(), now);
        }
        for ((queue, key), claim) in tx.claims {
            if let Some(q) = self.queues.get_mut(&queue) {
                q.set_claim(&key, claim);
            }
        }
        for (name, record) in &tx.queues {
            if record.is_none() {
                self.queues.remove(name);
            }
        }
        self.counters += tx.counters;
        for (queue, metric) in tx.metrics {
            if let Some(q) = self.queues.get_mut(&queue) {
                q.record_metric(now, metric);
            }
        }
        for sample in tx.latencies {
            self.latency.observe(sample);
        }
        for (worker, outcome) in &tx.outcomes {
            self.workers.record(worker, *outcome, now);
        }
        if let Some(worker) = &tx.touched {
            if self.workers.seen(worker, None, now) {
                let queues = self
                    .workers
                    .get(worker)
                    .map(|w| w.queues.clone())
                    .unwrap_or_default();
                events.push(EventKind::WorkerSeen {
                    worker: worker.clone(),
                    queues,
                });
            }
        }
        (
            events,
            Effects {
                finished: tx.finished,
                notify: tx.notify,
            },
        )
    }
}
