//! [`QueueEngine`]: the shared handle every part of belay talks to.
//!
//! The engine is split by concern into submodules that all add methods to
//! the same type: opening and index rebuild (`open`), enqueueing
//! (`enqueue`), leasing with long polls (`lease`), the worker protocol
//! (`work`), administrative changes (`admin`), reads (`read`) and the
//! periodic `tick`.
//!
//! Every mutating method follows the same shape, implemented once in
//! [`QueueEngine::transact`]:
//!
//! 1. take the engine lock and read the clock;
//! 2. stage the change in a [`Ctx`], reading jobs from the store as needed;
//! 3. write the staged [`Txn`](crate::txn::Txn) as one batch;
//! 4. apply it to the in-memory state and emit its events;
//! 5. release the lock, then wake long polls and run `on_finished` hooks.
//!
//! A failure in 2 or 3 returns before 4, leaving memory untouched.

mod admin;
mod enqueue;
mod lease;
mod open;
mod read;
mod tick;
mod verify;
mod work;

pub use tick::TickReport;
pub use verify::VerifyReport;

use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use belay_common::sync::lock;
use belay_common::{error, SharedClock, Timestamp};
use belay_proto::{EventSink, Job};
use belay_store::Kv;

use crate::error::Result;
use crate::options::EngineOptions;
use crate::records::Layout;
use crate::signal::Signal;
use crate::state::{Core, State};
use crate::txn::{Ctx, Effects, Txn};

/// A callback run when a job reaches a terminal state.
type Hook = dyn Fn(&Job) + Send + Sync;

/// The job queue engine. Share it as `Arc<QueueEngine>`; every method takes
/// `&self`.
///
/// # Locking
///
/// One mutex (the *engine lock*) guards all in-memory state, and every
/// store write happens while it is held, in the order the changes are
/// made. That makes each operation atomic and linearizable with respect to
/// all others, and it means the indexes can never disagree with the store:
/// there is no window in which another thread sees a change in one but not
/// the other. The price is that store writes are serialized. With the
/// log-structured store that is an in-memory table update plus a log
/// append; only under `SyncPolicy::Always` does each commit also wait for
/// an fsync. Operations that change many jobs (batch enqueue, multi-job
/// leases, tick, purge) write one batch for many changes to amortize that.
///
/// Per-queue locks were considered and rejected: most deployments have a
/// few queues with one hot one, so sharding by queue would not relieve the
/// hot path, while listing, job lookups by id, uniqueness and multi-queue
/// leases would all need cross-shard coordination.
///
/// Lock order, for the few places that hold more than one:
///
/// 1. the engine lock;
/// 2. the store's internal locks (inside `Kv` calls);
/// 3. the event sink's lock (sinks are called under the engine lock so
///    events are emitted in commit order, as `EventSink` allows).
///
/// Two locks are only ever taken *without* the engine lock held: the
/// long-poll signal (a waiting lease request sleeps holding only that) and
/// the hook list. Hooks and wakeups run after the engine lock is released,
/// so a hook may call back into the engine.
///
/// Reads by id ([`get`](Self::get)) go straight to the store without the
/// engine lock: every write is one atomic batch, so a read sees a job
/// either before or after any change, never halfway.
pub struct QueueEngine {
    kv: Arc<dyn Kv>,
    clock: SharedClock,
    sink: Arc<dyn EventSink>,
    opts: EngineOptions,
    layout: Layout,
    core: Mutex<Core>,
    signal: Signal,
    hooks: Mutex<Vec<Arc<Hook>>>,
    shutting_down: AtomicBool,
    waiting: AtomicUsize,
    lease_lost: AtomicU64,
}

impl fmt::Debug for QueueEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueueEngine")
            .field("kv", &self.kv)
            .field("options", &self.opts)
            .field("shutting_down", &self.is_shutting_down())
            .finish_non_exhaustive()
    }
}

impl QueueEngine {
    /// Registers `hook` to run for every job that reaches a terminal state
    /// (succeeded, dead or cancelled), after the change is persisted and
    /// outside the engine lock, so the hook may call back into the engine.
    /// This is how workflows learn that a step finished.
    ///
    /// Hooks run on the thread that caused the change, in registration
    /// order; a slow hook slows that caller down. A panicking hook is
    /// caught and logged. Jobs removed without finishing (purged, or
    /// deleted with their queue) do not trigger hooks.
    pub fn on_finished<F>(&self, hook: F)
    where
        F: Fn(&Job) + Send + Sync + 'static,
    {
        lock(&self.hooks).push(Arc::new(hook));
    }

    /// Stops handing out jobs: long-polling lease requests return
    /// immediately with what they have (nothing), and later lease calls
    /// fail with [`Error::ShuttingDown`](crate::Error::ShuttingDown).
    /// Everything else keeps working, so workers can still report the jobs
    /// they hold while the process drains.
    pub fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.signal.notify();
    }

    /// Whether [`shutdown`](Self::shutdown) has been called.
    #[must_use]
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    /// The options the engine was opened with.
    #[must_use]
    pub fn options(&self) -> &EngineOptions {
        &self.opts
    }

    /// The clock the engine reads.
    #[must_use]
    pub fn clock(&self) -> &SharedClock {
        &self.clock
    }

    fn lock(&self) -> MutexGuard<'_, Core> {
        lock(&self.core)
    }

    /// Stages, commits and applies one change under the engine lock at the
    /// clock's current time, then runs its effects.
    fn transact<T>(&self, stage: impl FnOnce(&mut Ctx<'_>) -> Result<T>) -> Result<T> {
        let (out, effects) = {
            let mut core = self.lock();
            let now = self.clock.now();
            self.transact_locked(&mut core, now, stage)?
        };
        self.after(effects);
        Ok(out)
    }

    /// Like [`transact`](Self::transact) for callers already holding the
    /// lock; the effects are returned for the caller to run after
    /// unlocking.
    fn transact_locked<T>(
        &self,
        core: &mut Core,
        now: Timestamp,
        stage: impl FnOnce(&mut Ctx<'_>) -> Result<T>,
    ) -> Result<(T, Effects)> {
        let Core { st, gen } = core;
        let mut cx = Ctx::new(st, gen, &*self.kv, &self.layout, now);
        let out = stage(&mut cx)?;
        let tx = cx.into_txn();
        let effects = self.commit(st, tx)?;
        Ok((out, effects))
    }

    /// Persists `tx`, then applies it and emits its events. Nothing in
    /// memory changes unless the write succeeds.
    fn commit(&self, st: &mut State, tx: Txn) -> Result<Effects> {
        let batch = tx.batch(&self.layout);
        if !batch.is_empty() {
            self.kv.write(batch)?;
        }
        let (events, effects) = st.apply(tx);
        for event in events {
            self.sink.emit(event);
        }
        Ok(effects)
    }

    /// Runs what a committed change leaves to do outside the lock.
    fn after(&self, effects: Effects) {
        if effects.notify {
            self.signal.notify();
        }
        if effects.finished.is_empty() {
            return;
        }
        let hooks: Vec<Arc<Hook>> = lock(&self.hooks).clone();
        for job in &effects.finished {
            for hook in &hooks {
                if catch_unwind(AssertUnwindSafe(|| hook(job))).is_err() {
                    error!("on_finished hook panicked"; job = %job.id, state = %job.state);
                }
            }
        }
    }
}
