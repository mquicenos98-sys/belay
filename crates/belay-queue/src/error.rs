//! What can go wrong in the queue engine, and how each failure is reported
//! to API clients.
//!
//! The variants are chosen by what a caller does next rather than by where
//! the failure happened: fix the request ([`Error::Invalid`]), give up on a
//! job ([`Error::LeaseLost`]), look elsewhere ([`Error::JobNotFound`]), or
//! retry later ([`Error::Store`], [`Error::ShuttingDown`]).

use std::fmt;

use belay_json::json;
use belay_proto::api::{ApiError, ErrorCode};
use belay_proto::{JobId, JobState, QueueName, TransitionError, Violations};
use belay_store::StoreError;

/// Any error returned by the queue engine.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The request broke one or more rules; nothing was changed.
    Invalid(Violations),
    /// No job has this id (it may have been deleted by retention).
    JobNotFound(JobId),
    /// No queue has this name.
    QueueNotFound(QueueName),
    /// The presented lease token is not the job's current one: the lease
    /// expired, the job was handed to another worker, or it no longer
    /// exists. The worker must abandon the job.
    LeaseLost(JobId),
    /// An enqueue with `on_conflict: reject` met a uniqueness key that is
    /// already held.
    UniqueConflict {
        /// The queue the key belongs to.
        queue: QueueName,
        /// The uniqueness key.
        key: String,
        /// The job holding it.
        holder: JobId,
    },
    /// The job is not in a state that allows the operation.
    WrongState {
        /// The job.
        job: JobId,
        /// Its current state.
        state: JobState,
        /// What was attempted, e.g. `"retry"`.
        action: &'static str,
        /// The states that would have allowed it, e.g. `"dead or cancelled"`.
        expected: &'static str,
    },
    /// The operation is not possible in the current situation, e.g.
    /// deleting a queue that still has pending jobs.
    FailedPrecondition(String),
    /// [`QueueEngine::shutdown`](crate::QueueEngine::shutdown) was called;
    /// no more jobs are handed out.
    ShuttingDown,
    /// The store failed to read or write. Nothing was changed in memory.
    Store(StoreError),
    /// A stored record could not be decoded or contradicts the engine's
    /// indexes.
    Corrupt {
        /// The record's key, printable.
        key: String,
        /// What is wrong with it.
        reason: String,
    },
    /// An invariant of the engine itself failed. Reported rather than
    /// panicking so that one bad request does not take the server down.
    Internal(String),
}

/// `Result` defaulting to this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// The API error code this error maps to.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Error::Invalid(_) => ErrorCode::InvalidArgument,
            Error::JobNotFound(_) | Error::QueueNotFound(_) => ErrorCode::NotFound,
            Error::LeaseLost(_) => ErrorCode::LeaseLost,
            Error::UniqueConflict { .. } => ErrorCode::AlreadyExists,
            Error::WrongState { .. } | Error::FailedPrecondition(_) => {
                ErrorCode::FailedPrecondition
            }
            Error::ShuttingDown => ErrorCode::Unavailable,
            Error::Store(e) => store_code(e),
            Error::Corrupt { .. } | Error::Internal(_) => ErrorCode::Internal,
        }
    }

    /// Whether the same call may succeed later without changing it.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        self.code().is_retryable()
    }

    pub(crate) fn corrupt(key: impl Into<String>, reason: impl fmt::Display) -> Error {
        Error::Corrupt {
            key: key.into(),
            reason: reason.to_string(),
        }
    }
}

fn store_code(e: &StoreError) -> ErrorCode {
    match e {
        StoreError::TooLarge { .. } => ErrorCode::InvalidArgument,
        StoreError::Corrupt { .. } | StoreError::InvalidOptions(_) => ErrorCode::Internal,
        // I/O failures, a closed or failed store, a held lock: all of them
        // may clear up (disk space freed, store reopened), so tell clients
        // to come back rather than to give up.
        _ => ErrorCode::Unavailable,
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Invalid(v) => v.fmt(f),
            Error::JobNotFound(id) => write!(f, "job {id} not found"),
            Error::QueueNotFound(name) => write!(f, "queue {name} not found"),
            Error::LeaseLost(id) => write!(
                f,
                "lease on job {id} is no longer held (expired or given to another worker)"
            ),
            Error::UniqueConflict { queue, key, holder } => write!(
                f,
                "unique key {key:?} in queue {queue} is held by job {holder}"
            ),
            Error::WrongState {
                job,
                state,
                action,
                expected,
            } => write!(
                f,
                "cannot {action} job {job}: it is {state}, expected {expected}"
            ),
            Error::FailedPrecondition(msg) | Error::Internal(msg) => f.write_str(msg),
            Error::ShuttingDown => f.write_str("queue engine is shutting down"),
            Error::Store(e) => write!(f, "storage error: {e}"),
            Error::Corrupt { key, reason } => write!(f, "corrupt record {key}: {reason}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Store(e) => Some(e),
            Error::Invalid(e) => Some(e),
            _ => None,
        }
    }
}

impl From<StoreError> for Error {
    fn from(e: StoreError) -> Error {
        Error::Store(e)
    }
}

impl From<Violations> for Error {
    fn from(v: Violations) -> Error {
        Error::Invalid(v)
    }
}

/// A transition the state machine refuses. The engine checks states before
/// transitioning, so reaching this means the stored record and the indexes
/// disagree.
impl From<TransitionError> for Error {
    fn from(e: TransitionError) -> Error {
        Error::Internal(format!("state machine refused a transition: {e}"))
    }
}

impl From<Error> for ApiError {
    fn from(e: Error) -> ApiError {
        match e {
            Error::Invalid(v) => ApiError::from(v),
            Error::JobNotFound(id) => ApiError::not_found("job", id),
            Error::QueueNotFound(name) => ApiError::not_found("queue", name),
            Error::LeaseLost(id) => ApiError::lease_lost(id),
            Error::UniqueConflict {
                ref queue,
                ref key,
                holder,
            } => ApiError::new(ErrorCode::AlreadyExists, e.to_string()).with_details(json!({
                "queue": queue.as_str(),
                "key": key.as_str(),
                "job": holder,
            })),
            Error::WrongState { job, state, .. } => ApiError::failed_precondition(e.to_string())
                .with_details(json!({"job": job, "state": state})),
            other => ApiError::new(other.code(), other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn id() -> JobId {
        "01HQ3K9Z8N6V2T4R5S7W0X1Y2Z".parse().expect("valid id")
    }

    #[test]
    fn messages_are_specific_and_lowercase() {
        let cases: Vec<(Error, &str)> = vec![
            (
                Error::JobNotFound(id()),
                "job 01HQ3K9Z8N6V2T4R5S7W0X1Y2Z not found",
            ),
            (
                Error::UniqueConflict {
                    queue: QueueName::from_static("emails"),
                    key: "user:42".into(),
                    holder: id(),
                },
                r#"unique key "user:42" in queue emails is held by job 01HQ3K9Z8N6V2T4R5S7W0X1Y2Z"#,
            ),
            (
                Error::WrongState {
                    job: id(),
                    state: JobState::Running,
                    action: "retry",
                    expected: "dead or cancelled",
                },
                "cannot retry job 01HQ3K9Z8N6V2T4R5S7W0X1Y2Z: it is running, expected dead or cancelled",
            ),
            (Error::ShuttingDown, "queue engine is shutting down"),
        ];
        for (err, text) in cases {
            assert_eq!(err.to_string(), text);
        }
    }

    #[test]
    fn api_errors_carry_the_right_code_and_details() {
        let lost = ApiError::from(Error::LeaseLost(id()));
        assert_eq!(lost.code, ErrorCode::LeaseLost);
        assert_eq!(lost.http_status(), 409);

        let conflict = ApiError::from(Error::UniqueConflict {
            queue: QueueName::from_static("emails"),
            key: "k".into(),
            holder: id(),
        });
        assert_eq!(conflict.code, ErrorCode::AlreadyExists);
        let details = conflict.details.expect("details");
        assert_eq!(details["key"], "k");
        assert_eq!(details["job"], "01HQ3K9Z8N6V2T4R5S7W0X1Y2Z");

        let missing = ApiError::from(Error::QueueNotFound(QueueName::from_static("q")));
        assert_eq!(missing.code, ErrorCode::NotFound);

        let state = ApiError::from(Error::WrongState {
            job: id(),
            state: JobState::Ready,
            action: "delete",
            expected: "succeeded, dead or cancelled",
        });
        assert_eq!(state.code, ErrorCode::FailedPrecondition);
        assert_eq!(state.details.expect("details")["state"], "ready");
    }

    #[test]
    fn store_failures_are_retryable_but_corruption_is_not() {
        let failed = Error::from(StoreError::Failed {
            reason: "disk full".into(),
        });
        assert_eq!(failed.code(), ErrorCode::Unavailable);
        assert!(failed.is_retryable());
        assert!(std::error::Error::source(&failed).is_some());

        let corrupt = Error::from(StoreError::Corrupt {
            file: PathBuf::from("wal-1.log"),
            offset: 7,
            reason: "bad crc".into(),
        });
        assert_eq!(corrupt.code(), ErrorCode::Internal);
        assert!(!corrupt.is_retryable());

        let record = Error::corrupt("job/01H", "missing field");
        assert_eq!(record.to_string(), "corrupt record job/01H: missing field");
        assert_eq!(ApiError::from(record).code, ErrorCode::Internal);
    }

    #[test]
    fn transition_errors_become_internal() {
        let e: Error = JobState::Succeeded
            .check_transition(JobState::Running)
            .unwrap_err()
            .into();
        assert_eq!(e.code(), ErrorCode::Internal);
        assert!(e.to_string().contains("succeeded to running"));
    }
}
