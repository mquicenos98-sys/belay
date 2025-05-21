//! [`JobContext`]: everything a handler gets besides its own state — the
//! job, its attempt and deadline, cancellation, progress reporting and
//! the [`Extensions`] that middleware fills in.
//!
//! Cancellation is cooperative. The worker sets the flag when a heartbeat
//! answer says someone asked to cancel the job, when the attempt runs past
//! its timeout, when the lease is lost, and when a shutdown's grace period
//! runs out; the handler notices through [`JobContext::is_cancelled`] or
//! [`JobContext::sleep_or_cancel`] and returns
//! [`HandlerError::cancelled`].

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use belay_common::sync::{lock, wait_timeout, Shutdown};
use belay_json::{FromJson, Value};
use belay_proto::limits::MAX_PROGRESS_MESSAGE_BYTES;
use belay_proto::{Job, JobId, Progress, QueueName, WorkerId};

use crate::extensions::Extensions;
use crate::handler::HandlerError;

/// Why a running job was asked to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CancelReason {
    /// Someone cancelled the job; the worker learnt it from a heartbeat.
    Requested,
    /// The attempt ran past its timeout.
    TimedOut,
    /// The worker is shutting down and the grace period is over.
    ShuttingDown,
    /// The lease was lost (it expired or another worker holds it), so any
    /// result will be dropped.
    LeaseLost,
}

impl fmt::Display for CancelReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CancelReason::Requested => "cancellation requested",
            CancelReason::TimedOut => "timed out",
            CancelReason::ShuttingDown => "worker shutting down",
            CancelReason::LeaseLost => "lease lost",
        })
    }
}
