//! Errors from setting a worker up. Once running, a worker as a whole never
//! fails: job failures, source outages and lost leases are reported to the
//! source, counted in [`WorkerStats`](crate::WorkerStats) and logged.

use std::fmt;
use std::io;

/// Why a worker could not be built or started.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// A [`WorkerConfig`](crate::WorkerConfig) value is out of range or
    /// inconsistent with another one.
    Config {
        /// The offending field.
        field: &'static str,
        /// What is wrong with it, e.g. `must be at least 1, got 0`.
        message: String,
    },
    /// The registry has no handlers, so every job would fail.
    NoHandlers,
    /// The same job kind was registered more than once.
    DuplicateKind(String),
    /// A kind's [`KindOptions`](crate::KindOptions) are invalid.
    KindOptions {
        /// The job kind.
        kind: String,
        /// What is wrong.
        message: String,
    },
    /// A worker thread could not be started.
    Spawn {
        /// The name of the thread.
        thread: String,
        /// The operating system's error.
        source: io::Error,
    },
}

/// `Result` with this crate's [`Error`] as the default error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub(crate) fn config(field: &'static str, message: impl Into<String>) -> Error {
        Error::Config {
            field,
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Config { field, message } => {
                write!(f, "invalid worker config: {field} {message}")
            }
            Error::NoHandlers => f.write_str("no job handlers registered"),
            Error::DuplicateKind(kind) => {
                write!(f, "handler for kind {kind:?} registered more than once")
            }
            Error::KindOptions { kind, message } => {
                write!(f, "invalid options for kind {kind:?}: {message}")
            }
            Error::Spawn { thread, source } => {
                write!(f, "cannot start thread {thread}: {source}")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Spawn { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn messages_name_the_problem() {
        assert_eq!(
            Error::config("concurrency", "must be at least 1, got 0").to_string(),
            "invalid worker config: concurrency must be at least 1, got 0"
        );
        assert_eq!(
            Error::DuplicateKind("resize".into()).to_string(),
            r#"handler for kind "resize" registered more than once"#
        );
        assert_eq!(
            Error::KindOptions {
                kind: "resize".into(),
                message: "concurrency must be at least 1".into(),
            }
            .to_string(),
            r#"invalid options for kind "resize": concurrency must be at least 1"#
        );
        assert_eq!(Error::NoHandlers.to_string(), "no job handlers registered");
    }

    #[test]
    fn spawn_errors_expose_their_cause() {
        let err = Error::Spawn {
            thread: "belay-worker-poll".into(),
            source: io::Error::new(io::ErrorKind::OutOfMemory, "no stack"),
        };
        assert_eq!(
            err.to_string(),
            "cannot start thread belay-worker-poll: no stack"
        );
        assert!(err.source().is_some());
        assert!(Error::NoHandlers.source().is_none());
    }
}
