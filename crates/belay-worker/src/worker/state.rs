//! State shared by the worker's threads.
//!
//! One mutex guards the table of owned jobs, the queues of jobs waiting to
//! start, the busy-slot count and the per-kind counts, so the capacity
//! arithmetic always sees a consistent picture. Two condition variables
//! hang off it: `work` wakes executor threads, `changed` wakes everyone
//! else (the poller waiting for a free slot, the watchdog waiting for the
//! next deadline, a shutdown waiting for the table to empty).
//!
//! Each job's own life is a small atomic state machine ([`Phase`]) so that
//! exactly one party reports it: the executor when the handler returns,
//! or whoever abandons it first (the heartbeat thread on a lost lease, the
//! watchdog on a timeout past the grace period, the shutdown sequence).
//! Source calls are never made while holding the lock.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use belay_common::sync::{lock, Shutdown};
use belay_proto::source::JobSource;
use belay_proto::{Job, JobId, LeaseToken};

use crate::context::JobSignals;
use crate::registry::{Entry, Registry};
use crate::stats::{Counters, WorkerStats};
use crate::worker::config::WorkerConfig;

/// Where a leased job is in its life on this worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Phase {
    /// Leased; waiting for a slot or for its kind's limit.
    Waiting = 0,
    /// The handler is running.
    Running = 1,
    /// The handler returned; its executor is reporting the result.
    Finishing = 2,
    /// Given up on; whoever abandoned it reports (or cannot, for a lost
    /// lease), and a late handler result is dropped.
    Abandoned = 3,
}

impl Phase {
    fn from_u8(v: u8) -> Phase {
        match v {
            0 => Phase::Waiting,
            1 => Phase::Running,
            2 => Phase::Finishing,
            _ => Phase::Abandoned,
        }
    }
}

/// The time budget of a running attempt.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timing {
    pub(crate) timeout: Option<Duration>,
    pub(crate) deadline: Option<Instant>,
}

/// A job this worker holds a lease on.
pub(crate) struct Tracked {
    pub(crate) job: Arc<Job>,
    pub(crate) token: LeaseToken,
    pub(crate) entry: Arc<Entry>,
    pub(crate) signals: Arc<JobSignals>,
    phase: AtomicU8,
    timing: Mutex<Option<Timing>>,
    /// Set by the watchdog when the deadline passes, so the timeout is
    /// counted and logged once.
    pub(crate) deadline_hit: AtomicBool,
}

impl Tracked {
    pub(crate) fn new(job: Job, token: LeaseToken, entry: Arc<Entry>) -> Tracked {
        Tracked {
            job: Arc::new(job),
            token,
            entry,
            signals: Arc::new(JobSignals::default()),
            phase: AtomicU8::new(Phase::Waiting as u8),
            timing: Mutex::new(None),
            deadline_hit: AtomicBool::new(false),
        }
    }

    pub(crate) fn id(&self) -> JobId {
        self.job.id
    }

    pub(crate) fn kind(&self) -> &str {
        &self.entry.kind
    }

    pub(crate) fn phase(&self) -> Phase {
        Phase::from_u8(self.phase.load(Ordering::SeqCst))
    }

    fn transition(&self, from: Phase, to: Phase) -> bool {
        self.phase
            .compare_exchange(from as u8, to as u8, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// Waiting → Running, recording the deadline. Fails if the job was
    /// abandoned while it waited.
    pub(crate) fn begin(&self, timing: Timing) -> bool {
        // Timing first: the watchdog only looks at running jobs, and must
        // find the deadline when it does.
        *lock(&self.timing) = Some(timing);
        self.transition(Phase::Waiting, Phase::Running)
    }

    /// Running → Finishing. Fails if the job was abandoned meanwhile, in
    /// which case the result must be dropped.
    pub(crate) fn finish(&self) -> bool {
        self.transition(Phase::Running, Phase::Finishing)
    }

    /// Waiting or Running → Abandoned. Fails if the job is already
    /// finishing (its executor will report) or abandoned.
    pub(crate) fn abandon(&self) -> bool {
        self.transition(Phase::Waiting, Phase::Abandoned)
            || self.transition(Phase::Running, Phase::Abandoned)
    }

    pub(crate) fn timing(&self) -> Option<Timing> {
        *lock(&self.timing)
    }
}

/// Everything guarded by [`Shared::state`].
#[derive(Default)]
pub(crate) struct State {
    /// Jobs leased and not yet reported or abandoned.
    pub(crate) jobs: HashMap<JobId, Arc<Tracked>>,
    /// Jobs cleared to run, waiting for an executor thread.
    pub(crate) ready: VecDeque<Arc<Tracked>>,
    /// Jobs whose kind is at its concurrency limit.
    pub(crate) parked: VecDeque<Arc<Tracked>>,
    /// Per kind: jobs in `ready` plus handlers running (abandoned ones
    /// included, since they still use whatever the limit protects).
    pub(crate) active: HashMap<String, usize>,
    /// Executor threads running a handler or reporting its result.
    pub(crate) busy: usize,
    /// Executor threads that have exited.
    pub(crate) exited: usize,
    /// Cleared when shutdown begins: the poller hands back what it gets.
    pub(crate) accepting: bool,
    /// Executor threads exit once `ready` is empty.
    pub(crate) closing: bool,
    /// The watchdog exits.
    pub(crate) stopping: bool,
}

impl State {
    fn active_for(&self, kind: &str) -> usize {
        self.active.get(kind).copied().unwrap_or(0)
    }

    /// Whether a job of `entry`'s kind may start now.
    pub(crate) fn kind_has_room(&self, entry: &Entry) -> bool {
        entry
            .options
            .concurrency
            .map_or(true, |max| self.active_for(&entry.kind) < max)
    }

    /// Clears `job` to run: it counts against its kind from now on.
    pub(crate) fn dispatch(&mut self, job: Arc<Tracked>) {
        *self.active.entry(job.kind().to_owned()).or_insert(0) += 1;
        self.ready.push_back(job);
    }

    /// A handler of `kind` returned (or a cleared job will never run).
    /// Returns whether a parked job was cleared to run as a result.
    pub(crate) fn release_kind(&mut self, kind: &str) -> bool {
        if let Some(n) = self.active.get_mut(kind) {
            *n = n.saturating_sub(1);
        }
        self.promote()
    }

    /// Moves parked jobs whose kind has room to `ready`, oldest first.
    fn promote(&mut self) -> bool {
        let mut promoted = false;
        let mut i = 0;
        while i < self.parked.len() {
            if self.kind_has_room(&self.parked[i].entry) {
                if let Some(job) = self.parked.remove(i) {
                    self.dispatch(job);
                    promoted = true;
                }
            } else {
                i += 1;
            }
        }
        promoted
    }

    /// Drops `id` from the table and the waiting queues. A job that was
    /// cleared to run but not yet picked up gives its kind slot back.
    pub(crate) fn forget(&mut self, id: JobId) {
        self.jobs.remove(&id);
        self.parked.retain(|t| t.id() != id);
        if let Some(pos) = self.ready.iter().position(|t| t.id() == id) {
            if let Some(job) = self.ready.remove(pos) {
                self.release_kind(job.kind());
            }
        }
    }

    /// Empties both waiting queues for a shutdown. Cleared jobs give their
    /// kind slots back; nothing is promoted, since nothing new may start.
    pub(crate) fn take_unstarted(&mut self) -> Vec<Arc<Tracked>> {
        let mut taken: Vec<Arc<Tracked>> = self.parked.drain(..).collect();
        while let Some(job) = self.ready.pop_front() {
            if let Some(n) = self.active.get_mut(job.kind()) {
                *n = n.saturating_sub(1);
            }
            taken.push(job);
        }
        taken
    }

    /// Slots neither executing nor promised to a leased job.
    pub(crate) fn free_slots(&self, concurrency: usize) -> usize {
        concurrency.saturating_sub(self.busy + self.ready.len() + self.parked.len())
    }

    /// How many jobs to ask the source for.
    ///
    /// Lease requests cannot name kinds, so any job may come back. The
    /// answer is the free slots, further capped by the room left under
    /// per-kind limits when every registered kind has one (asking for more
    /// could only produce jobs that have to wait). While a leased job is
    /// already waiting for its kind, nothing more is asked for: the head
    /// of the queue is evidently a saturated kind, and more of it would
    /// only sit here holding leases other workers could use.
    pub(crate) fn lease_capacity(&self, concurrency: usize, registry: &Registry) -> usize {
        if !self.parked.is_empty() {
            return 0;
        }
        let free = self.free_slots(concurrency);
        if free == 0 || registry.has_unlimited_kind() {
            return free;
        }
        let room: usize = registry
            .entries()
            .map(|e| {
                let max = e.options.concurrency.unwrap_or(usize::MAX);
                max.saturating_sub(self.active_for(&e.kind))
            })
            .fold(0, usize::saturating_add);
        free.min(room)
    }

    /// Jobs leased but not running.
    pub(crate) fn waiting(&self) -> usize {
        self.jobs
            .values()
            .filter(|t| t.phase() == Phase::Waiting)
            .count()
    }
}

/// What every worker thread holds.
pub(crate) struct Shared {
    pub(crate) config: WorkerConfig,
    pub(crate) source: Arc<dyn JobSource>,
    pub(crate) registry: Registry,
    pub(crate) counters: Counters,
    pub(crate) state: Mutex<State>,
    pub(crate) work: Condvar,
    pub(crate) changed: Condvar,
    /// Triggered when shutdown begins; handlers see it through
    /// `JobContext::is_shutting_down` and backoff waits end early.
    pub(crate) draining: Shutdown,
}

impl Shared {
    pub(crate) fn new(
        source: Arc<dyn JobSource>,
        registry: Registry,
        config: WorkerConfig,
    ) -> Shared {
        Shared {
            config,
            source,
            registry,
            counters: Counters::default(),
            state: Mutex::new(State {
                accepting: true,
                ..State::default()
            }),
            work: Condvar::new(),
            changed: Condvar::new(),
            draining: Shutdown::new(),
        }
    }

    /// Removes a job that has been reported or abandoned.
    pub(crate) fn forget(&self, id: JobId) {
        lock(&self.state).forget(id);
        self.work.notify_all();
        self.changed.notify_all();
    }

    pub(crate) fn stats(&self) -> WorkerStats {
        let mut stats = self.counters.snapshot();
        let state = lock(&self.state);
        stats.running = state.jobs.len();
        stats.waiting = state.waiting();
        stats.busy_slots = state.busy;
        stats.concurrency = self.config.concurrency;
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::{handler_fn, Outcome};
    use crate::registry::KindOptions;
    use crate::JobContext;
    use belay_common::Timestamp;
    use belay_proto::JobSpec;

    fn registry(limits: &[(&str, Option<usize>)]) -> Registry {
        limits.iter().fold(Registry::new(), |r, (kind, limit)| {
            let options = KindOptions {
                concurrency: *limit,
                timeout: None,
            };
            r.register_with(
                *kind,
                handler_fn(|_: &mut JobContext| Ok(Outcome::done())),
                options,
            )
        })
    }

    fn tracked(registry: &Registry, kind: &str) -> Arc<Tracked> {
        let job = Job::new(
            JobId::generate(),
            JobSpec::new("q", kind),
            Timestamp::from_secs(1),
        );
        let entry = registry.get(kind).expect("kind registered").clone();
        Arc::new(Tracked::new(job, LeaseToken::new(1), entry))
    }

    fn timing() -> Timing {
        Timing {
            timeout: None,
            deadline: None,
        }
    }

    #[test]
    fn phases_allow_exactly_one_reporter() {
        let r = registry(&[("a", None)]);
        let t = tracked(&r, "a");
        assert_eq!(t.phase(), Phase::Waiting);
        assert!(t.begin(timing()));
        assert!(!t.begin(timing()), "cannot start twice");
        assert!(t.finish());
        assert!(!t.abandon(), "a finishing job is reported by its executor");

        let t = tracked(&r, "a");
        assert!(t.begin(timing()));
        assert!(t.abandon());
        assert!(!t.finish(), "the late result is dropped");
        assert!(!t.abandon());

        let t = tracked(&r, "a");
        assert!(t.abandon(), "waiting jobs can be abandoned");
        assert!(!t.begin(timing()));
    }

    #[test]
    fn saturated_kinds_park_and_are_promoted_in_order() {
        let r = registry(&[("slow", Some(1)), ("fast", None)]);
        let mut state = State::default();
        let first = tracked(&r, "slow");
        let second = tracked(&r, "slow");
        assert!(state.kind_has_room(&first.entry));
        state.dispatch(first.clone());
        assert!(!state.kind_has_room(&second.entry));
        state.parked.push_back(second.clone());
        assert_eq!(
            state.lease_capacity(4, &r),
            0,
            "no leasing while a job is parked"
        );

        state.ready.pop_front();
        assert!(state.release_kind("slow"), "the parked job is cleared");
        assert!(state.parked.is_empty());
        assert_eq!(state.ready.front().map(|t| t.id()), Some(second.id()));
        assert_eq!(state.active_for("slow"), 1);
    }

    #[test]
    fn capacity_is_bounded_by_limits_when_every_kind_has_one() {
        let limited = registry(&[("a", Some(2)), ("b", Some(1))]);
        let mut state = State::default();
        assert_eq!(state.lease_capacity(8, &limited), 3);
        state.active.insert("a".into(), 2);
        state.busy = 2;
        assert_eq!(state.lease_capacity(8, &limited), 1);
        state.active.insert("b".into(), 1);
        state.busy = 3;
        assert_eq!(state.lease_capacity(8, &limited), 0);

        let mixed = registry(&[("a", Some(1)), ("b", None)]);
        let mut state = State::default();
        state.active.insert("a".into(), 1);
        state.busy = 1;
        assert_eq!(state.lease_capacity(4, &mixed), 3);
        state.busy = 4;
        assert_eq!(state.lease_capacity(4, &mixed), 0);
    }

    #[test]
    fn forgetting_a_cleared_job_releases_its_kind_slot() {
        let r = registry(&[("a", Some(1))]);
        let mut state = State::default();
        let cleared = tracked(&r, "a");
        let parked = tracked(&r, "a");
        state.jobs.insert(cleared.id(), cleared.clone());
        state.jobs.insert(parked.id(), parked.clone());
        state.dispatch(cleared.clone());
        state.parked.push_back(parked.clone());

        state.forget(cleared.id());
        assert!(!state.jobs.contains_key(&cleared.id()));
        assert_eq!(state.ready.len(), 1, "the parked job took the freed slot");
        state.forget(parked.id());
        assert!(state.ready.is_empty() && state.parked.is_empty() && state.jobs.is_empty());
        assert_eq!(state.active_for("a"), 0);
    }

    #[test]
    fn shutdown_takes_waiting_jobs_without_touching_running_counts() {
        // Regression: parked jobs never held a kind slot, and giving one
        // back for them undercounted the handler still running.
        let r = registry(&[("a", Some(1)), ("b", None)]);
        let mut state = State::default();
        let running = tracked(&r, "a");
        state.dispatch(running.clone());
        state.ready.pop_front();
        let parked = tracked(&r, "a");
        state.parked.push_back(parked.clone());
        let cleared = tracked(&r, "b");
        state.dispatch(cleared.clone());

        let taken: Vec<JobId> = state.take_unstarted().iter().map(|t| t.id()).collect();
        assert_eq!(taken, [parked.id(), cleared.id()]);
        assert_eq!(state.active_for("a"), 1, "the running handler still counts");
        assert_eq!(state.active_for("b"), 0);
        assert!(state.ready.is_empty() && state.parked.is_empty());
    }
}
