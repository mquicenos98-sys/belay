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
