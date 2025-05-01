//! What the engine keeps in the store, and under which keys.
//!
//! | namespace | key | value |
//! |---|---|---|
//! | `job` | ULID (16 bytes, so keys sort by creation) | the [`Job`] as JSON |
//! | `queue` | queue name | [`QueueRecord`]: configuration and pause flag |
//! | `uniq` | queue name, uniqueness key | [`Claim`]: holder and expiry |
//! | `qmeta` | `schema` | format version of the records above |
//!
//! Each namespace is a [`Namespace::child`] of the engine's configured
//! root. Nothing else is persisted: the per-queue indexes (ready sets,
//! timers, lease expiries, counts) are derived from the job documents when
//! the engine opens, so a state change writes exactly the documents it
//! changes and there is no secondary index that could drift from them.
//!
//! Claims are persisted separately rather than derived because a
//! `for:<duration>` uniqueness scope outlives its job: the key stays taken
//! for the whole period even after retention has deleted the holder.

use belay_common::Timestamp;
use belay_json::{FromJson, FromJsonError, ObjectBuilder, ObjectReader, ToJson, Value};
use belay_proto::wire::{BuilderExt, ReaderExt};
use belay_proto::{Job, JobId, JobState, QueueConfig, QueueName};
use belay_store::keys::{KeyReader, Printable};
use belay_store::{KeyRange, Namespace};

use crate::error::{Error, Result};

/// Version of the record formats written by this crate. Bumped when a
/// change needs a migration; older engines refuse newer stores.
pub(crate) const SCHEMA_VERSION: u32 = 1;

/// The engine's key layout under its root namespace.
#[derive(Clone, Debug)]
pub(crate) struct Layout {
    jobs: Namespace,
    queues: Namespace,
    claims: Namespace,
    meta: Namespace,
}

impl Layout {
    pub(crate) fn new(root: &Namespace) -> Layout {
        Layout {
            jobs: root.child("job"),
            queues: root.child("queue"),
            claims: root.child("uniq"),
            meta: root.child("qmeta"),
        }
    }

    pub(crate) fn job_key(&self, id: JobId) -> Vec<u8> {
        self.jobs.key_buf().ulid(id.ulid()).into_vec()
    }

    pub(crate) fn job_id_of(&self, key: &[u8]) -> Option<JobId> {
        let mut r = KeyReader::after_prefix(key, self.jobs.prefix())?;
        let id = r.ulid().ok()?;
        r.expect_end().ok()?;
        Some(JobId::from_ulid(id))
    }

    pub(crate) fn jobs(&self) -> KeyRange {
        self.jobs.all()
    }

    pub(crate) fn queue_key(&self, name: &QueueName) -> Vec<u8> {
        self.queues.key_buf().str(name.as_str()).into_vec()
    }

    pub(crate) fn queue_name_of(&self, key: &[u8]) -> Option<QueueName> {
        let mut r = KeyReader::after_prefix(key, self.queues.prefix())?;
        let name = r.str().ok()?;
        r.expect_end().ok()?;
        QueueName::new(name).ok()
    }

    pub(crate) fn queues(&self) -> KeyRange {
        self.queues.all()
    }

    pub(crate) fn claim_key(&self, queue: &QueueName, key: &str) -> Vec<u8> {
        self.claims
            .key_buf()
            .str(queue.as_str())
            .str(key)
            .into_vec()
    }

    /// Prefix of every claim in `queue`.
    pub(crate) fn claim_prefix(&self, queue: &QueueName) -> Vec<u8> {
        self.claims.key_buf().str(queue.as_str()).into_vec()
    }

    pub(crate) fn claim_of(&self, key: &[u8]) -> Option<(QueueName, String)> {
        let mut r = KeyReader::after_prefix(key, self.claims.prefix())?;
        let queue = r.str().ok()?;
        let unique = r.str().ok()?;
        r.expect_end().ok()?;
        Some((QueueName::new(queue).ok()?, unique))
    }

    pub(crate) fn claims(&self) -> KeyRange {
        self.claims.all()
    }

    pub(crate) fn schema_key(&self) -> Vec<u8> {
        self.meta.key_buf().str("schema").into_vec()
    }
}

/// A key in printable form, for error messages.
pub(crate) fn printable(key: &[u8]) -> String {
    Printable(key).to_string()
}

pub(crate) fn encode<T: ToJson + ?Sized>(value: &T) -> Vec<u8> {
    belay_json::to_string_of(value).into_bytes()
}

pub(crate) fn decode<T: FromJson>(key: &[u8], bytes: &[u8]) -> Result<T> {
    belay_json::from_bytes(bytes).map_err(|e| Error::corrupt(printable(key), e))
}

/// Decodes a job document and checks that it is stored under its own id.
pub(crate) fn decode_job(layout: &Layout, key: &[u8], bytes: &[u8]) -> Result<Job> {
    let job: Job = decode(key, bytes)?;
    match layout.job_id_of(key) {
        Some(id) if id == job.id => Ok(job),
        Some(id) => Err(Error::corrupt(
            printable(key),
            format!("document of job {} stored under id {id}", job.id),
        )),
        None => Err(Error::corrupt(printable(key), "not a job key")),
    }
}

/// A queue's persisted settings. Queues are recorded on first use, so they
/// survive restarts even while empty.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct QueueRecord {
    pub(crate) config: QueueConfig,
    pub(crate) paused: bool,
}

impl ToJson for QueueRecord {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("config", &self.config)
            .field("paused", self.paused)
            .build()
    }
}

impl FromJson for QueueRecord {
    /// Written by the engine itself, so unknown fields are tolerated: a
    /// store written by a newer version still opens.
    fn from_json(value: &Value) -> Result<QueueRecord, FromJsonError> {
        let r = ObjectReader::new(value, "QueueRecord")?;
        Ok(QueueRecord {
            config: r.or_default("config")?,
            paused: r.or_default("paused")?,
        })
    }
}

/// Who holds a uniqueness key, and until when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Claim {
    /// The holder.
    pub(crate) job: JobId,
    /// For `for:<duration>` scopes, when the key is released whatever
    /// happens to the job. Other scopes release with the job's state.
    pub(crate) until: Option<Timestamp>,
    /// The holder's last state, recorded when the holder was deleted while
    /// the key was still claimed, so a duplicate enqueue can still report
    /// it.
    pub(crate) holder_state: Option<JobState>,
}
