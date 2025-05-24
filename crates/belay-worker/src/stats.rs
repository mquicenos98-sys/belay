//! What a worker has done: lock-free counters updated by the worker's
//! threads and the [`WorkerStats`] snapshot built from them.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use belay_common::sync::lock;

/// Longest `last_error` kept, in bytes; source errors can quote bodies.
const MAX_LAST_ERROR_BYTES: usize = 512;

/// A point-in-time view of a worker.
///
/// Counters only grow. `failed` counts every attempt reported as failed
/// (whatever the reason: handler error, panic, timeout, unknown kind,
/// shutdown); `retried` is the part of those reported as retryable, so
/// the source may run them again if attempts remain.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkerStats {
    /// Jobs received from the source.
    pub leased: u64,
    /// Attempts completed and acknowledged by the source.
    pub succeeded: u64,
    /// Attempts reported as failed.
    pub failed: u64,
    /// Failed attempts reported as retryable.
    pub retried: u64,
    /// Jobs given up on while their handler was still running (lease lost,
    /// timeout past the grace period, shutdown past the grace period) or
    /// whose lease was lost before the result could be reported. Their
    /// results, if any ever arrive, are dropped.
    pub abandoned: u64,
    /// Handler panics.
    pub panicked: u64,
    /// Attempts that ran past their timeout.
    pub timed_out: u64,
    /// Jobs whose cancellation was requested while they ran.
    pub cancelled: u64,
    /// Jobs failed back to the source as "worker shutting down".
    pub handed_back: u64,
    /// Lease calls that failed.
    pub lease_errors: u64,
    /// Heartbeat calls that failed (other than for a lost lease).
    pub heartbeat_errors: u64,
    /// Results that could not be delivered even after retrying.
    pub report_errors: u64,
    /// Jobs leased and not yet reported, including ones waiting to start.
    pub running: usize,
    /// Of `running`, jobs waiting for an executor slot or for their kind's
    /// concurrency limit.
    pub waiting: usize,
    /// Executor threads busy with a handler or with reporting its result.
    /// Can exceed `running - waiting` while abandoned handlers are still
    /// running.
    pub busy_slots: usize,
    /// Executor threads.
    pub concurrency: usize,
    /// The most recent error from the source or a handler, abridged.
    pub last_error: Option<String>,
}

impl WorkerStats {
    /// Executor threads not doing anything.
    #[must_use]
    pub fn idle_slots(&self) -> usize {
        self.concurrency.saturating_sub(self.busy_slots)
    }

    /// Attempts that ended one way or the other.
    #[must_use]
    pub fn finished(&self) -> u64 {
        self.succeeded + self.failed
    }
}

/// `leased=12 succeeded=9 failed=2 retried=1 abandoned=0 running=1/4`,
/// with the rarer counters only when they are non-zero.
impl fmt::Display for WorkerStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "leased={} succeeded={} failed={} retried={} abandoned={} running={}/{}",
            self.leased,
            self.succeeded,
            self.failed,
            self.retried,
            self.abandoned,
            self.running,
            self.concurrency
        )?;
        let optional = [
            ("panicked", self.panicked),
            ("timed_out", self.timed_out),
            ("cancelled", self.cancelled),
            ("handed_back", self.handed_back),
            ("lease_errors", self.lease_errors),
            ("heartbeat_errors", self.heartbeat_errors),
            ("report_errors", self.report_errors),
        ];
        for (name, value) in optional {
            if value > 0 {
                write!(f, " {name}={value}")?;
            }
        }
        Ok(())
    }
}

/// The worker's counters. Gauges (`running`, `busy_slots`, ...) are read
/// from the worker's state when a snapshot is taken.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    pub(crate) leased: AtomicU64,
    pub(crate) succeeded: AtomicU64,
    pub(crate) failed: AtomicU64,
    pub(crate) retried: AtomicU64,
    pub(crate) abandoned: AtomicU64,
    pub(crate) panicked: AtomicU64,
    pub(crate) timed_out: AtomicU64,
    pub(crate) cancelled: AtomicU64,
    pub(crate) handed_back: AtomicU64,
    pub(crate) lease_errors: AtomicU64,
    pub(crate) heartbeat_errors: AtomicU64,
    pub(crate) report_errors: AtomicU64,
    last_error: Mutex<Option<String>>,
}

impl Counters {
    /// Adds one to `counter`.
    pub(crate) fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Remembers `error` as the latest one.
    pub(crate) fn note_error(&self, error: impl fmt::Display) {
        let mut text = error.to_string();
        crate::context::truncate_at_char(&mut text, MAX_LAST_ERROR_BYTES);
        *lock(&self.last_error) = Some(text);
    }

    /// The counters as a snapshot; the caller fills in the gauges.
    pub(crate) fn snapshot(&self) -> WorkerStats {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        WorkerStats {
            leased: get(&self.leased),
            succeeded: get(&self.succeeded),
            failed: get(&self.failed),
            retried: get(&self.retried),
            abandoned: get(&self.abandoned),
            panicked: get(&self.panicked),
            timed_out: get(&self.timed_out),
            cancelled: get(&self.cancelled),
            handed_back: get(&self.handed_back),
            lease_errors: get(&self.lease_errors),
            heartbeat_errors: get(&self.heartbeat_errors),
            report_errors: get(&self.report_errors),
            last_error: lock(&self.last_error).clone(),
            ..WorkerStats::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_shows_rare_counters_only_when_set() {
        let mut stats = WorkerStats {
            leased: 12,
            succeeded: 9,
            failed: 2,
            retried: 1,
            running: 1,
            concurrency: 4,
            ..WorkerStats::default()
        };
        assert_eq!(
            stats.to_string(),
            "leased=12 succeeded=9 failed=2 retried=1 abandoned=0 running=1/4"
        );
        stats.panicked = 1;
        stats.lease_errors = 3;
        assert!(stats.to_string().ends_with(" panicked=1 lease_errors=3"));
    }

    #[test]
    fn derived_numbers() {
        let stats = WorkerStats {
            succeeded: 3,
            failed: 2,
            busy_slots: 5,
            concurrency: 4,
            ..WorkerStats::default()
        };
        assert_eq!(stats.finished(), 5);
        assert_eq!(stats.idle_slots(), 0, "abandoned handlers can overcommit");
    }

    #[test]
    fn counters_snapshot_and_abridge_errors() {
        let counters = Counters::default();
        Counters::bump(&counters.leased);
        Counters::bump(&counters.leased);
        Counters::bump(&counters.report_errors);
        counters.note_error("x".repeat(2 * MAX_LAST_ERROR_BYTES));
        let stats = counters.snapshot();
        assert_eq!((stats.leased, stats.report_errors), (2, 1));
        assert_eq!(
            stats.last_error.map(|e| e.len()),
            Some(MAX_LAST_ERROR_BYTES)
        );
    }
}
