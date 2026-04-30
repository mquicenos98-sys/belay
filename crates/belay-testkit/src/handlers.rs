//! Recording handlers: [`Recorder`], [`Execution`], [`Behavior`], [`Gate`]
//! and [`RecordingHandler`], which ties them together.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use belay_common::hash::hash64;
use belay_common::sync::{lock, wait_timeout};
use belay_json::Value;
use belay_proto::{JobId, QueueName, WorkerId};
use belay_worker::{Handler, HandlerError, JobContext, Outcome};

use crate::error::{Error, Result};

/// How often waiting handlers look at cancellation and shutdown.
const CHECK_EVERY: Duration = Duration::from_millis(5);

/// How an attempt ended, as far as the handler knows.
#[derive(Clone, Debug, PartialEq)]
pub enum Recorded {
    /// Still running.
    Running,
    /// Returned success with this result.
    Succeeded(Option<Value>),
    /// Returned an error.
    Failed {
        /// The error message.
        message: String,
        /// Whether it asked for a retry.
        retryable: bool,
    },
    /// Stopped because the job was cancelled (on request or by timeout).
    Cancelled,
}

/// One attempt of one job, as seen by a recording handler.
#[derive(Clone, Debug, PartialEq)]
pub struct Execution {
    /// Order of the start among everything this recorder saw, from 0.
    pub seq: u64,
    /// The job.
    pub job: JobId,
    /// Its kind.
    pub kind: String,
    /// Its queue.
    pub queue: QueueName,
    /// The server's attempt number (restarts at 1 after an operator retry).
    pub attempt: u32,
    /// The worker that ran it.
    pub worker: WorkerId,
    /// The payload, as resolved by the server (workflow placeholders
    /// already replaced).
    pub payload: Value,
    /// When the handler started.
    pub started: Instant,
    /// How long it ran; `None` while running.
    pub took: Option<Duration>,
    /// What it returned.
    pub outcome: Recorded,
}
