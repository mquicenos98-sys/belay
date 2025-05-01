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

impl Claim {
    pub(crate) fn new(job: JobId, until: Option<Timestamp>) -> Claim {
        Claim {
            job,
            until,
            holder_state: None,
        }
    }

    /// Whether a timed claim has run out at `now`.
    pub(crate) fn expired(&self, now: Timestamp) -> bool {
        self.until.is_some_and(|until| until <= now)
    }
}

impl ToJson for Claim {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("job", self.job)
            .opt_timestamp("until", self.until)
            .opt_field("holder_state", &self.holder_state)
            .build()
    }
}

impl FromJson for Claim {
    fn from_json(value: &Value) -> Result<Claim, FromJsonError> {
        let r = ObjectReader::new(value, "Claim")?;
        Ok(Claim {
            job: r.req("job")?,
            until: r.opt_timestamp("until")?,
            holder_state: r.opt("holder_state")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_json::json;
    use belay_proto::JobSpec;
    use std::time::Duration;

    fn layout() -> Layout {
        Layout::new(&Namespace::root())
    }

    #[test]
    fn keys_round_trip_and_stay_in_their_namespace() {
        let l = layout();
        let id: JobId = "01HQ3K9Z8N6V2T4R5S7W0X1Y2Z".parse().expect("id");
        let key = l.job_key(id);
        assert_eq!(l.job_id_of(&key), Some(id));
        assert!(l.jobs().contains(&key));
        assert!(!l.queues().contains(&key));

        let q = QueueName::from_static("billing:invoices");
        assert_eq!(l.queue_name_of(&l.queue_key(&q)), Some(q.clone()));
        let claim = l.claim_key(&q, "invoice\u{0}7");
        assert_eq!(
            l.claim_of(&claim),
            Some((q.clone(), "invoice\u{0}7".into()))
        );
        assert!(claim.starts_with(&l.claim_prefix(&q)));
        assert!(!l
            .claim_key(&QueueName::from_static("billing"), "x")
            .starts_with(&l.claim_prefix(&q)));
        assert_eq!(l.job_id_of(&l.queue_key(&q)), None);
    }

    #[test]
    fn job_keys_sort_by_creation_time() {
        let l = layout();
        let early = JobId::from_ulid(belay_common::Ulid::from_parts(1_000, 99));
        let late = JobId::from_ulid(belay_common::Ulid::from_parts(2_000, 1));
        assert!(l.job_key(early) < l.job_key(late));
    }

    #[test]
    fn nested_roots_keep_engines_apart() {
        let a = Layout::new(&Namespace::new("tenant-a"));
        let b = Layout::new(&Namespace::new("tenant-b"));
        let id = JobId::generate();
        assert_ne!(a.job_key(id), b.job_key(id));
        assert!(!b.jobs().contains(&a.job_key(id)));
    }

    #[test]
    fn job_documents_must_live_under_their_own_id() {
        let l = layout();
        let job = Job::new(
            JobId::generate(),
            JobSpec::new("q", "k"),
            Timestamp::from_secs(5),
        );
        let bytes = encode(&job);
        assert_eq!(
            decode_job(&l, &l.job_key(job.id), &bytes).expect("decodes"),
            job
        );
        let other = l.job_key(JobId::generate());
        let err = decode_job(&l, &other, &bytes).unwrap_err();
        assert!(err.to_string().contains("stored under id"), "{err}");
        let err = decode_job(&l, &l.job_key(job.id), b"{not json").unwrap_err();
        assert!(matches!(err, Error::Corrupt { .. }), "{err}");
    }

    #[test]
    fn queue_records_round_trip_and_tolerate_new_fields() {
        let mut record = QueueRecord::default();
        record.paused = true;
        record.config.max_concurrency = Some(4);
        record.config.retention.succeeded = Duration::from_secs(60);
        let v = record.to_json();
        assert_eq!(QueueRecord::from_json(&v), Ok(record.clone()));
        let mut extended = v.clone();
        extended.as_object_mut().expect("object").insert("shard", 3);
        assert_eq!(QueueRecord::from_json(&extended), Ok(record));
        assert_eq!(
            QueueRecord::from_json(&json!({})),
            Ok(QueueRecord::default())
        );
    }

    #[test]
    fn claims_round_trip_with_optional_parts() {
        let id = JobId::generate();
        let plain = Claim::new(id, None);
        assert_eq!(plain.to_json(), json!({"job": id}));
        let timed = Claim {
            until: Some(Timestamp::from_secs(60)),
            holder_state: Some(JobState::Succeeded),
            ..plain.clone()
        };
        for c in [plain, timed.clone()] {
            assert_eq!(Claim::from_json(&c.to_json()), Ok(c));
        }
        assert!(timed.expired(Timestamp::from_secs(60)));
        assert!(!timed.expired(Timestamp::from_millis(59_999)));
        assert!(!Claim::new(id, None).expired(Timestamp::MAX));
    }
}
