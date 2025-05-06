//! Staging a state change: [`Ctx`] collects every write an operation needs
//! into a [`Txn`], which is persisted as one atomic [`WriteBatch`] and only
//! then applied to memory.
//!
//! Operations never touch the committed [`State`] directly. They read it
//! through a `Ctx`, which overlays the changes staged so far, so that an
//! operation touching several jobs (a batch enqueue whose second job
//! replaces the first, a lease handing out ten jobs) sees its own earlier
//! steps. If staging fails, or the store rejects the batch, the `Txn` is
//! dropped and memory is exactly as before.
//!
//! Uniqueness claims follow the jobs automatically: staging a job whose
//! state no longer holds its key releases the claim, and deleting a job
//! either releases its claim or, for a timed scope that has not run out,
//! records the holder's final state in it.

use std::collections::BTreeMap;

use belay_common::{Rng, Timestamp};
use belay_proto::event::EventKind;
use belay_proto::{
    Job, JobId, JobState, LeaseToken, QueueConfig, QueueName, UniqueScope, WorkerId,
};
use belay_store::{Kv, WriteBatch};

use crate::error::{Error, Result};
use crate::queue::bucket::TokenBucket;
use crate::queue::fair::FairCursor;
use crate::queue::throughput::Metric;
use crate::records::{decode_job, encode, printable, Claim, Layout, QueueRecord};
use crate::state::{Gen, State};
use crate::stats::{Counters, LeaseLatency};
use crate::workers::Outcome;

/// A lease plan's end position for one queue, installed on commit.
#[derive(Debug)]
pub(crate) struct Plan {
    pub(crate) queue: QueueName,
    pub(crate) cursor: FairCursor,
    pub(crate) bucket: Option<TokenBucket>,
}

/// Everything one operation changes.
#[derive(Debug)]
pub(crate) struct Txn {
    pub(crate) now: Timestamp,
    /// New job versions; `None` deletes.
    pub(crate) jobs: BTreeMap<JobId, Option<Job>>,
    /// Claim changes; `None` releases.
    pub(crate) claims: BTreeMap<(QueueName, String), Option<Claim>>,
    /// Queue records; `None` drops the queue with its claims.
    pub(crate) queues: BTreeMap<QueueName, Option<QueueRecord>>,
    pub(crate) plans: Vec<Plan>,
    pub(crate) round_robin: Option<usize>,
    pub(crate) events: Vec<EventKind>,
    /// Jobs that reached a terminal state, for the `on_finished` hooks.
    pub(crate) finished: Vec<Job>,
    /// Whether waiting lease requests should look again.
    pub(crate) notify: bool,
    pub(crate) counters: Counters,
    pub(crate) metrics: Vec<(QueueName, Metric)>,
    pub(crate) latencies: Vec<LeaseLatency>,
    pub(crate) outcomes: Vec<(WorkerId, Outcome)>,
    /// A worker that called in, for the registry.
    pub(crate) touched: Option<WorkerId>,
}

impl Txn {
    fn new(now: Timestamp) -> Txn {
        Txn {
            now,
            jobs: BTreeMap::new(),
            claims: BTreeMap::new(),
            queues: BTreeMap::new(),
            plans: Vec::new(),
            round_robin: None,
            events: Vec::new(),
            finished: Vec::new(),
            notify: false,
            counters: Counters::default(),
            metrics: Vec::new(),
            latencies: Vec::new(),
            outcomes: Vec::new(),
            touched: None,
        }
    }

    /// The store writes of this transaction, as one batch.
    pub(crate) fn batch(&self, layout: &Layout) -> WriteBatch {
        let mut batch = WriteBatch::with_capacity(self.jobs.len() + self.claims.len());
        // Queue drops come first: their prefix deletion of claims must not
        // wipe claims written later in the same batch.
        for (name, record) in &self.queues {
            match record {
                Some(record) => {
                    batch.put(layout.queue_key(name), encode(record));
                }
                None => {
                    batch
                        .delete(layout.queue_key(name))
                        .delete_prefix(layout.claim_prefix(name));
                }
            }
        }
        for (id, job) in &self.jobs {
            match job {
                Some(job) => batch.put(layout.job_key(*id), encode(job)),
                None => batch.delete(layout.job_key(*id)),
            };
        }
        for ((queue, key), claim) in &self.claims {
            match claim {
                Some(claim) => batch.put(layout.claim_key(queue, key), encode(claim)),
                None => batch.delete(layout.claim_key(queue, key)),
            };
        }
        batch
    }
}

/// What an applied transaction leaves to do once the engine lock is
/// released.
#[derive(Debug, Default)]
pub(crate) struct Effects {
    pub(crate) finished: Vec<Job>,
    pub(crate) notify: bool,
}

impl Effects {
    pub(crate) fn merge(&mut self, other: Effects) {
        self.finished.extend(other.finished);
        self.notify |= other.notify;
    }
}

/// A read view of the committed state with staged changes on top, plus
/// the staging operations.
pub(crate) struct Ctx<'a> {
    st: &'a State,
    gen: &'a mut Gen,
    kv: &'a dyn Kv,
    layout: &'a Layout,
    pub(crate) now: Timestamp,
    tx: Txn,
}

impl<'a> Ctx<'a> {
    pub(crate) fn new(
        st: &'a State,
        gen: &'a mut Gen,
        kv: &'a dyn Kv,
        layout: &'a Layout,
        now: Timestamp,
    ) -> Ctx<'a> {
        Ctx {
            st,
            gen,
            kv,
            layout,
            now,
            tx: Txn::new(now),
        }
    }

    /// The committed state underneath.
    pub(crate) fn state(&self) -> &'a State {
        self.st
    }

    pub(crate) fn into_txn(self) -> Txn {
        self.tx
    }

    // --- jobs -------------------------------------------------------------

    /// The job as staged, or as stored if not staged; `None` if it does
    /// not exist (or is staged for deletion).
    pub(crate) fn job(&self, id: JobId) -> Result<Option<Job>> {
        if let Some(staged) = self.tx.jobs.get(&id) {
            return Ok(staged.clone());
        }
        let Some(meta) = self.st.jobs.get(&id) else {
            return Ok(None);
        };
        let key = self.layout.job_key(id);
        let bytes = self
            .kv
            .get(&key)?
            .ok_or_else(|| Error::corrupt(printable(&key), "indexed job has no stored document"))?;
        let job = decode_job(self.layout, &key, &bytes)?;
        if job.state != meta.state {
            return Err(Error::corrupt(
                printable(&key),
                format!("stored as {} but indexed as {}", job.state, meta.state),
            ));
        }
        Ok(Some(job))
    }

    /// Like [`job`](Self::job), failing with `JobNotFound`.
    pub(crate) fn require(&self, id: JobId) -> Result<Job> {
        self.job(id)?.ok_or(Error::JobNotFound(id))
    }

    /// The job's state, staged or committed, without reading the store.
    pub(crate) fn job_state(&self, id: JobId) -> Option<JobState> {
        match self.tx.jobs.get(&id) {
            Some(staged) => staged.as_ref().map(|job| job.state),
            None => self.st.jobs.get(&id).map(|meta| meta.state),
        }
    }

    /// Stages a new version of `job`, releasing its uniqueness claim if its
    /// state no longer holds it.
    pub(crate) fn put(&mut self, job: Job) {
        if let Some(unique) = &job.spec.unique {
            let untimed = !matches!(unique.scope, UniqueScope::For(_));
            if untimed && !unique.scope.holds(job.state, job.created_at, self.now) {
                let held = self
                    .claim(&job.spec.queue, &unique.key)
                    .is_some_and(|c| c.job == job.id);
                if held {
                    self.set_claim(&job.spec.queue, &unique.key, None);
                }
            }
        }
        self.tx.jobs.insert(job.id, Some(job));
    }

    /// Stages the deletion of `job`. Its claim is released, unless it is
    /// timed and still running, in which case it remembers the job's final
    /// state for duplicate enqueues to report.
    pub(crate) fn delete(&mut self, job: &Job) {
        if let Some(unique) = &job.spec.unique {
            if let Some(claim) = self
                .claim(&job.spec.queue, &unique.key)
                .filter(|c| c.job == job.id)
            {
                let keep = claim.until.is_some_and(|until| until > self.now);
                let next = keep.then(|| Claim {
                    holder_state: Some(job.state),
                    ..claim
                });
                self.set_claim(&job.spec.queue, &unique.key, next);
            }
        }
        self.tx.jobs.insert(job.id, None);
    }

    // --- claims -----------------------------------------------------------

    /// The claim on `key` in `queue`, staged or committed.
    pub(crate) fn claim(&self, queue: &QueueName, key: &str) -> Option<Claim> {
        match self.tx.claims.get(&(queue.clone(), key.to_owned())) {
            Some(staged) => staged.clone(),
            None => {
                if self.dropped(queue) {
                    return None;
                }
                self.st.queues.get(queue)?.claim(key).cloned()
            }
        }
    }

    pub(crate) fn set_claim(&mut self, queue: &QueueName, key: &str, claim: Option<Claim>) {
        self.tx
            .claims
            .insert((queue.clone(), key.to_owned()), claim);
    }

    // --- queues -----------------------------------------------------------

    fn dropped(&self, queue: &QueueName) -> bool {
        matches!(self.tx.queues.get(queue), Some(None))
    }

    /// The queue's record, staged or committed.
    pub(crate) fn queue_record(&self, name: &QueueName) -> Option<QueueRecord> {
        match self.tx.queues.get(name) {
            Some(staged) => staged.clone(),
            None => self.st.queues.get(name).map(|q| q.record()),
        }
    }

    /// The queue's configuration, or the default one for a queue that does
    /// not exist.
    pub(crate) fn queue_config(&self, name: &QueueName) -> QueueConfig {
        self.queue_record(name)
            .map(|r| r.config)
            .unwrap_or_default()
    }

    /// The queue's record, creating the queue with the default
    /// configuration if it does not exist yet.
    pub(crate) fn ensure_queue(&mut self, name: &QueueName) -> QueueRecord {
        match self.queue_record(name) {
            Some(record) => record,
            None => {
                let record = QueueRecord::default();
                self.put_queue(name, record.clone());
                record
            }
        }
    }

    pub(crate) fn put_queue(&mut self, name: &QueueName, record: QueueRecord) {
        self.tx.queues.insert(name.clone(), Some(record));
    }

    /// Stages the removal of the queue record and every claim in it. The
    /// caller removes the queue's jobs first.
    pub(crate) fn drop_queue(&mut self, name: &QueueName) {
        self.tx.claims.retain(|(queue, _), _| queue != name);
        self.tx.queues.insert(name.clone(), None);
    }

    // --- scheduling and randomness ------------------------------------------

    pub(crate) fn plan(&mut self, plan: Plan) {
        self.tx.plans.push(plan);
    }

    pub(crate) fn set_round_robin(&mut self, next: usize) {
        self.tx.round_robin = Some(next);
    }

    pub(crate) fn job_id(&mut self) -> Result<JobId> {
        self.gen.job_id(self.now)
    }

    pub(crate) fn token(&mut self) -> LeaseToken {
        LeaseToken::generate(&mut self.gen.rng)
    }

    pub(crate) fn rng(&mut self) -> &mut Rng {
        &mut self.gen.rng
    }

    // --- side effects -------------------------------------------------------

    pub(crate) fn emit(&mut self, event: EventKind) {
        self.tx.events.push(event);
    }

    pub(crate) fn finished(&mut self, job: Job) {
        self.tx.finished.push(job);
    }

    pub(crate) fn notify(&mut self) {
        self.tx.notify = true;
    }

    pub(crate) fn counters(&mut self) -> &mut Counters {
        &mut self.tx.counters
    }

    pub(crate) fn metric(&mut self, queue: &QueueName, metric: Metric) {
        self.tx.metrics.push((queue.clone(), metric));
    }

    pub(crate) fn latency(&mut self, sample: LeaseLatency) {
        self.tx.latencies.push(sample);
    }

    pub(crate) fn outcome(&mut self, worker: &WorkerId, outcome: Outcome) {
        self.tx.outcomes.push((worker.clone(), outcome));
    }

    pub(crate) fn touch(&mut self, worker: &WorkerId) {
        self.tx.touched = Some(worker.clone());
    }
}
