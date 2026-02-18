//! Errors from starting and stopping an [`Engine`](crate::Engine).
//!
//! Requests served by a running engine fail with
//! [`ApiError`](belay_proto::api::ApiError) instead, ready for the HTTP
//! layer; this type is for the process around it: a configuration that
//! does not load, a store that does not open, a configured schedule that
//! does not apply, a gossip socket that cannot be bound.

use std::fmt;
use std::io;

use belay_proto::api::ApiError;

/// Anything that can stop an engine from starting.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The configuration is invalid.
    Config(crate::config::ConfigError),
    /// The store could not be opened or failed.
    Store(belay_store::StoreError),
    /// The queue engine could not be opened.
    Queue(belay_queue::Error),
    /// The scheduler or the workflow engine could not be opened.
    Scheduler(belay_scheduler::Error),
    /// The cluster node could not be started.
    Cluster(belay_cluster::Error),
    /// Metric families could not be registered (a name clash with families
    /// already in a registry passed to the builder).
    Metrics(belay_metrics::Error),
    /// The embedded worker could not be started.
    Worker(belay_worker::Error),
    /// Something from the configuration could not be applied at startup:
    /// `what` names it (`schedule nightly-report`).
    Apply {
        /// What was being applied.
        what: String,
        /// Why it was refused.
        source: ApiError,
    },
    /// An operating-system call failed.
    Io {
        /// What was being done, e.g. `binding the gossip socket 0.0.0.0:7946`.
        context: String,
        /// The underlying error.
        source: io::Error,
    },
}

/// `Result` defaulting to this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub(crate) fn io(context: impl Into<String>, source: io::Error) -> Error {
        Error::Io {
            context: context.into(),
            source,
        }
    }

    pub(crate) fn apply(what: impl Into<String>, source: impl Into<ApiError>) -> Error {
        Error::Apply {
            what: what.into(),
            source: source.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Configuration errors render as multi-line diagnostics of
            // their own; adding a prefix would only get in the way.
            Error::Config(e) => e.fmt(f),
            Error::Store(e) => write!(f, "opening the store failed: {e}"),
            Error::Queue(e) => write!(f, "opening the queue engine failed: {e}"),
            Error::Scheduler(e) => write!(f, "opening the scheduler failed: {e}"),
            Error::Cluster(e) => write!(f, "starting the cluster node failed: {e}"),
            Error::Metrics(e) => write!(f, "registering metrics failed: {e}"),
            Error::Worker(e) => write!(f, "starting the embedded worker failed: {e}"),
            Error::Apply { what, source } => {
                write!(f, "applying {what} from the configuration failed: {source}")
            }
            Error::Io { context, source } => write!(f, "{context} failed: {source}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Config(e) => Some(e),
            Error::Store(e) => Some(e),
            Error::Queue(e) => Some(e),
            Error::Scheduler(e) => Some(e),
            Error::Cluster(e) => Some(e),
            Error::Metrics(e) => Some(e),
            Error::Worker(e) => Some(e),
            Error::Apply { source, .. } => Some(source),
            Error::Io { source, .. } => Some(source),
        }
    }
}

impl From<crate::config::ConfigError> for Error {
    fn from(e: crate::config::ConfigError) -> Error {
        Error::Config(e)
    }
}

impl From<belay_store::StoreError> for Error {
    fn from(e: belay_store::StoreError) -> Error {
        Error::Store(e)
    }
}

impl From<belay_queue::Error> for Error {
    fn from(e: belay_queue::Error) -> Error {
        Error::Queue(e)
    }
}

impl From<belay_scheduler::Error> for Error {
    fn from(e: belay_scheduler::Error) -> Error {
        Error::Scheduler(e)
    }
}

impl From<belay_cluster::Error> for Error {
    fn from(e: belay_cluster::Error) -> Error {
        Error::Cluster(e)
    }
}

impl From<belay_metrics::Error> for Error {
    fn from(e: belay_metrics::Error) -> Error {
        Error::Metrics(e)
    }
}

impl From<belay_worker::Error> for Error {
    fn from(e: belay_worker::Error) -> Error {
        Error::Worker(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_say_what_failed() {
        let e = Error::apply("schedule nightly", ApiError::invalid_argument("bad cron"));
        assert_eq!(
            e.to_string(),
            "applying schedule nightly from the configuration failed: invalid_argument: bad cron"
        );
        let e = Error::io(
            "binding the gossip socket 0.0.0.0:7946",
            io::Error::new(io::ErrorKind::AddrInUse, "address in use"),
        );
        assert_eq!(
            e.to_string(),
            "binding the gossip socket 0.0.0.0:7946 failed: address in use"
        );
        assert!(std::error::Error::source(&e).is_some());
    }
}
