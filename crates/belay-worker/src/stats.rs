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
