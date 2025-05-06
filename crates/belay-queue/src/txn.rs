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
