//! The kit's [`Error`]: what can go wrong setting up a harness or waiting
//! for the system under test, with the upstream error kept as the source.

use std::fmt;
use std::io;
use std::time::Duration;

use belay_proto::api::ApiError;

/// Everything a harness or a wait can fail with.
///
/// Test functions can return [`Result`] and use `?` on harness calls, API
/// calls through the typed client and engine calls alike: each upstream
/// error converts into the matching variant.
#[derive(Debug)]
pub enum Error {
    /// The engine did not start.
    Engine(belay_core::Error),
    /// The HTTP server did not start.
    Server(belay_server::Error),
    /// A call through the typed HTTP client failed.
    Client(belay_client::Error),
    /// An engine call (or a job source call) answered with an error.
    Api(ApiError),
    /// A store could not be opened or closed.
    Store(belay_store::StoreError),
    /// A worker could not be configured or started.
    Worker(belay_worker::Error),
    /// A socket, thread or temporary directory could not be set up.
    Io(io::Error),
    /// A condition did not hold within the time allowed.
    Timeout {
        /// What was being waited for.
        what: String,
        /// How long the wait lasted.
        waited: Duration,
    },
    /// The harness cannot do this in its current setup, e.g. advancing a
    /// system clock or crashing a store that lives on a real disk.
    Unsupported(String),
    /// An argument was rejected before anything was sent, e.g. a queue
    /// name that is not valid.
    Invalid(String),
}

/// `Result` with the kit's [`Error`] as the default error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// A [`Timeout`](Error::Timeout) for `what` after `waited`.
    #[must_use]
    pub fn timeout(what: impl Into<String>, waited: Duration) -> Error {
        Error::Timeout {
            what: what.into(),
            waited,
        }
    }

    /// Whether this is a [`Timeout`](Error::Timeout).
    #[must_use]
    pub fn is_timeout(&self) -> bool {
        matches!(self, Error::Timeout { .. })
    }

    /// The API error behind this one, whether it came back from the engine
    /// or over HTTP.
    #[must_use]
    pub fn api_error(&self) -> Option<&ApiError> {
        match self {
            Error::Api(e) => Some(e),
            Error::Client(e) => e.api_error(),
            _ => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Engine(e) => write!(f, "engine: {e}"),
            Error::Server(e) => write!(f, "server: {e}"),
            Error::Client(e) => write!(f, "client: {e}"),
            Error::Api(e) => write!(f, "api: {e}"),
            Error::Store(e) => write!(f, "store: {e}"),
            Error::Worker(e) => write!(f, "worker: {e}"),
            Error::Io(e) => write!(f, "i/o: {e}"),
            Error::Timeout { what, waited } => write!(
                f,
                "timed out after {} waiting for {what}",
                belay_common::format_duration(*waited)
            ),
            Error::Unsupported(what) => write!(f, "unsupported: {what}"),
            Error::Invalid(what) => write!(f, "invalid argument: {what}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Engine(e) => Some(e),
            Error::Server(e) => Some(e),
            Error::Client(e) => Some(e),
            Error::Api(e) => Some(e),
            Error::Store(e) => Some(e),
            Error::Worker(e) => Some(e),
            Error::Io(e) => Some(e),
            Error::Timeout { .. } | Error::Unsupported(_) | Error::Invalid(_) => None,
        }
    }
}

impl From<belay_core::Error> for Error {
    fn from(e: belay_core::Error) -> Error {
        Error::Engine(e)
    }
}

impl From<belay_server::Error> for Error {
    fn from(e: belay_server::Error) -> Error {
        Error::Server(e)
    }
}

impl From<belay_client::Error> for Error {
    fn from(e: belay_client::Error) -> Error {
        Error::Client(e)
    }
}

impl From<ApiError> for Error {
    fn from(e: ApiError) -> Error {
        Error::Api(e)
    }
}

impl From<belay_store::StoreError> for Error {
    fn from(e: belay_store::StoreError) -> Error {
        Error::Store(e)
    }
}

impl From<belay_worker::Error> for Error {
    fn from(e: belay_worker::Error) -> Error {
        Error::Worker(e)
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(e)
    }
}
