//! The seams between belay's parts: [`JobSource`], through which a worker
//! gets and reports jobs whether the queue is in-process (belay-core's
//! engine) or remote (belay-client over HTTP), and [`EventSink`], through
//! which engines report what happens.

use std::fmt;
use std::sync::{Arc, Mutex};

use belay_common::sync::lock;

use crate::api::{
    ApiError, CompleteRequest, FailRequest, HeartbeatRequest, HeartbeatResponse, LeaseRequest,
    LeasedJob,
};
use crate::event::EventKind;
use crate::ids::JobId;

/// Where a worker leases jobs and reports on them.
///
/// Implementations enforce the lease protocol: every call about a job
/// presents the token from [`lease`](Self::lease), and a stale token fails
/// with [`ErrorCode::LeaseLost`](crate::api::ErrorCode::LeaseLost), upon
/// which the worker must drop the job without reporting further.
///
/// ```
/// use std::sync::Mutex;
/// use belay_proto::api::*;
/// use belay_proto::source::JobSource;
/// use belay_proto::JobId;
///
/// /// A source with nothing to hand out that records failures.
/// struct Recorder(Mutex<Vec<String>>);
///
/// impl JobSource for Recorder {
///     fn lease(&self, _: &LeaseRequest) -> Result<Vec<LeasedJob>, ApiError> {
///         Ok(Vec::new())
///     }
///     fn heartbeat(&self, id: &JobId, _: &HeartbeatRequest) -> Result<HeartbeatResponse, ApiError> {
///         Err(ApiError::lease_lost(*id))
///     }
///     fn complete(&self, id: &JobId, _: &CompleteRequest) -> Result<(), ApiError> {
///         Err(ApiError::lease_lost(*id))
///     }
///     fn fail(&self, _: &JobId, req: &FailRequest) -> Result<(), ApiError> {
///         self.0.lock().unwrap().push(req.error.clone());
///         Ok(())
///     }
/// }
/// ```
pub trait JobSource: Send + Sync {
    /// Leases up to `req.max_jobs` jobs, long-polling up to `req.wait`.
    fn lease(&self, req: &LeaseRequest) -> Result<Vec<LeasedJob>, ApiError>;

    /// Renews the lease on `id` and records progress.
    fn heartbeat(&self, id: &JobId, req: &HeartbeatRequest) -> Result<HeartbeatResponse, ApiError>;

    /// Reports success.
    fn complete(&self, id: &JobId, req: &CompleteRequest) -> Result<(), ApiError>;

    /// Reports a failed attempt.
    fn fail(&self, id: &JobId, req: &FailRequest) -> Result<(), ApiError>;
}

macro_rules! forward_job_source {
    ($($ptr:ty),*) => {$(
        impl<T: JobSource + ?Sized> JobSource for $ptr {
            fn lease(&self, req: &LeaseRequest) -> Result<Vec<LeasedJob>, ApiError> {
                (**self).lease(req)
            }

            fn heartbeat(
                &self,
                id: &JobId,
                req: &HeartbeatRequest,
            ) -> Result<HeartbeatResponse, ApiError> {
                (**self).heartbeat(id, req)
            }

            fn complete(&self, id: &JobId, req: &CompleteRequest) -> Result<(), ApiError> {
                (**self).complete(id, req)
            }

            fn fail(&self, id: &JobId, req: &FailRequest) -> Result<(), ApiError> {
                (**self).fail(id, req)
            }
        }
    )*};
}

forward_job_source!(&T, Box<T>, Arc<T>);

/// Receives events from engines. Implementations must be cheap and must
/// not block for long: engines emit while holding their own locks.
pub trait EventSink: Send + Sync {
    /// Records that `kind` happened. The receiver stamps time and sequence.
    fn emit(&self, kind: EventKind);
}

macro_rules! forward_event_sink {
    ($($ptr:ty),*) => {$(
        impl<T: EventSink + ?Sized> EventSink for $ptr {
            fn emit(&self, kind: EventKind) {
                (**self).emit(kind);
            }
        }
    )*};
}

forward_event_sink!(&T, Box<T>, Arc<T>);

/// Discards every event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NoopSink;

impl EventSink for NoopSink {
    fn emit(&self, _: EventKind) {}
}

impl NoopSink {
    /// A shared no-op sink, for engines that take `Arc<dyn EventSink>`.
    #[must_use]
    pub fn shared() -> Arc<dyn EventSink> {
        Arc::new(NoopSink)
    }
}

/// Keeps every event in memory, for tests.
///
/// ```
/// use belay_proto::event::EventKind;
/// use belay_proto::source::{CollectingSink, EventSink};
/// use belay_proto::QueueName;
///
/// let sink = CollectingSink::new();
/// sink.emit(EventKind::QueuePaused { queue: QueueName::from_static("emails") });
/// assert_eq!(sink.type_names(), ["queue.paused"]);
/// assert_eq!(sink.take().len(), 1);
/// assert!(sink.is_empty());
/// ```
#[derive(Debug, Default)]
pub struct CollectingSink {
    events: Mutex<Vec<EventKind>>,
}
