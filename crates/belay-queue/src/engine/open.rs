//! Opening an engine over a store: schema check, then rebuilding every
//! in-memory index from the stored records.
//!
//! Rebuilding by scanning is the price of persisting nothing but the
//! documents themselves (see [`records`](crate::records)): opening costs
//! one decode per job, in exchange for a write path that touches one key
//! per changed job and indexes that cannot drift from the data. The scan runs in chunks so the store never
//! materializes the whole job table at once.
//!
//! Small inconsistencies that an older version or a hand-edited store
//! could leave behind are repaired rather than refused, in one batch at the
//! end, with a warning: a job whose queue has no record gets one, and
//! claims whose holder no longer holds them are released.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use belay_common::{info, warn, SharedClock, Timestamp};
use belay_proto::{EventSink, JobId, JobState, QueueName, UniqueScope};
use belay_store::{Kv, WriteBatch};

use super::QueueEngine;
use crate::error::{Error, Result};
use crate::options::EngineOptions;
use crate::records::{
    decode, decode_job, encode, printable, Claim, Layout, QueueRecord, SCHEMA_VERSION,
};
use crate::signal::Signal;
use crate::state::{Core, Gen, State};

/// What a uniqueness claim is checked against at open.
pub(super) struct Holder {
    queue: QueueName,
    key: String,
    scope: UniqueScope,
    state: JobState,
    created_at: Timestamp,
}

impl QueueEngine {
    /// Opens the engine over `kv`, rebuilding its indexes from the stored
    /// jobs, queues and uniqueness claims.
    ///
    /// `clock` supplies every timestamp the engine records and compares;
    /// `sink` receives an event for every state change.
    ///
    /// ```
    /// use std::sync::Arc;
    /// use belay_common::{ManualClock, Timestamp};
    /// use belay_proto::source::NoopSink;
    /// use belay_proto::JobSpec;
    /// use belay_queue::{EngineOptions, QueueEngine};
    /// use belay_store::MemKv;
    ///
    /// let clock = ManualClock::new(Timestamp::from_secs(1_700_000_000));
    /// let kv = Arc::new(MemKv::new());
    /// let engine = QueueEngine::open(kv.clone(), clock.shared(), NoopSink::shared(), EngineOptions::new())?;
    /// let id = engine.enqueue(JobSpec::new("emails", "send"))?.id;
    /// drop(engine);
    ///
    /// // Everything is rebuilt from the store.
    /// let engine = QueueEngine::open(kv, clock.shared(), NoopSink::shared(), EngineOptions::new())?;
    /// assert_eq!(engine.get(&id)?.kind(), "send");
    /// # Ok::<(), belay_queue::Error>(())
    /// ```
    pub fn open(
        kv: Arc<dyn Kv>,
        clock: SharedClock,
        sink: Arc<dyn EventSink>,
        opts: EngineOptions,
    ) -> Result<QueueEngine> {
        let started = Instant::now();
        let layout = Layout::new(&opts.namespace);
        check_schema(&*kv, &layout)?;
        let now = clock.now();
        let mut st = State::new(opts.throughput_window, opts.latency_samples);
        let mut repair = WriteBatch::new();

        load_queues(&*kv, &layout, &mut st, now)?;
        let holders = load_jobs(&*kv, &layout, &mut st, &mut repair, now, opts.open_chunk)?;
        let claims = load_claims(&*kv, &layout, &mut st, &holders, &mut repair, now)?;
        if !repair.is_empty() {
            warn!(
                "repairing queue records and releasing stale uniqueness claims";
                ops = repair.len() as u64
            );
            kv.write(repair)?;
        }
        info!(
            "queue engine opened";
            jobs = st.jobs.len() as u64,
            queues = st.queues.len() as u64,
            claims = claims as u64,
            elapsed = started.elapsed()
        );

        let newest = st.jobs.keys().next_back().copied();
        Ok(QueueEngine {
            core: Mutex::new(Core {
                st,
                gen: Gen::new(opts.seed, newest),
            }),
            kv,
            clock,
            sink,
            opts,
            layout,
            signal: Signal::new(),
            hooks: Mutex::new(Vec::new()),
            shutting_down: AtomicBool::new(false),
            waiting: AtomicUsize::new(0),
            lease_lost: AtomicU64::new(0),
        })
    }
}

/// Records the schema version in a new store and refuses stores written
/// by a newer, incompatible version.
fn check_schema(kv: &dyn Kv, layout: &Layout) -> Result<()> {
    let key = layout.schema_key();
    match kv.get(&key)? {
        None => {
            kv.put(&key, SCHEMA_VERSION.to_string().as_bytes())?;
            Ok(())
        }
        Some(bytes) => {
            let version: u32 = std::str::from_utf8(&bytes)
                .ok()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| Error::corrupt(printable(&key), "unreadable schema version"))?;
            if version > SCHEMA_VERSION {
                return Err(Error::FailedPrecondition(format!(
                    "store holds queue records in format {version}, but this version \
                     of belay only reads up to {SCHEMA_VERSION}; upgrade belay"
                )));
            }
            Ok(())
        }
    }
}

pub(super) fn load_queues(
    kv: &dyn Kv,
    layout: &Layout,
    st: &mut State,
    now: Timestamp,
) -> Result<()> {
    for (key, value) in kv.scan(layout.queues())? {
        let name = layout
            .queue_name_of(&key)
            .ok_or_else(|| Error::corrupt(printable(&key), "not a queue key"))?;
        let record: QueueRecord = decode(&key, &value)?;
        st.upsert_queue(&name, record, now);
    }
    Ok(())
}
