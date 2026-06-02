//! The error type of the benchmark harness: bad command lines, failures of
//! the belay components under test, and runs whose numbers would be
//! meaningless (a queue that ran dry, a worker that missed its deadline).

use std::fmt;
use std::io;

use belay_proto::api::ApiError;

/// Everything that can stop a benchmark run.
#[derive(Debug)]
pub enum Error {
    /// The command line is wrong; the message says how.
    Usage(String),
    /// A file or socket operation of the harness itself failed.
    Io {
        /// What the harness was doing.
        context: String,
        /// The underlying error.
        source: io::Error,
    },
    /// The durable store failed.
    Store(belay_store::Error),
    /// The queue engine refused an operation.
    Queue(belay_queue::Error),
    /// The belay engine failed to start or to start a worker.
    Engine(belay_core::Error),
    /// An engine API call failed.
    Api(ApiError),
    /// The HTTP server failed to start.
    Server(belay_server::Error),
    /// A call through the HTTP client failed.
    Client(belay_client::Error),
    /// The worker runtime refused its configuration or failed to start.
    Worker(belay_worker::Error),
    /// A cron expression or time zone of a scenario did not parse.
    Cron(belay_cron::CronError),
    /// The benchmark observed something that invalidates its numbers.
    Invalid(String),
}

/// `Result` with the harness [`Error`] as the default error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// An [`Error::Io`] with a description of what was being done.
    pub fn io(context: impl Into<String>, source: io::Error) -> Error {
        Error::Io {
            context: context.into(),
            source,
        }
    }

    /// The process exit code this error maps to: 2 for usage errors (as
    /// most command-line tools do), 1 for everything else.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::Usage(_) => 2,
            _ => 1,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Usage(msg) | Error::Invalid(msg) => f.write_str(msg),
            Error::Io { context, source } => write!(f, "{context}: {source}"),
            Error::Store(e) => write!(f, "store: {e}"),
            Error::Queue(e) => write!(f, "queue engine: {e}"),
            Error::Engine(e) => write!(f, "engine: {e}"),
            Error::Api(e) => write!(f, "engine api: {e}"),
            Error::Server(e) => write!(f, "server: {e}"),
            Error::Client(e) => write!(f, "http client: {e}"),
            Error::Worker(e) => write!(f, "worker: {e}"),
            Error::Cron(e) => write!(f, "cron: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Usage(_) | Error::Invalid(_) => None,
            Error::Io { source, .. } => Some(source),
            Error::Store(e) => Some(e),
            Error::Queue(e) => Some(e),
            Error::Engine(e) => Some(e),
            Error::Api(e) => Some(e),
            Error::Server(e) => Some(e),
            Error::Client(e) => Some(e),
            Error::Worker(e) => Some(e),
            Error::Cron(e) => Some(e),
        }
    }
}

macro_rules! from_error {
    ($($source:ty => $variant:ident),* $(,)?) => {$(
        impl From<$source> for Error {
            fn from(e: $source) -> Error {
                Error::$variant(e)
            }
        }
    )*};
}

from_error! {
    belay_store::Error => Store,
    belay_queue::Error => Queue,
    belay_core::Error => Engine,
    ApiError => Api,
    belay_server::Error => Server,
    belay_client::Error => Client,
    belay_worker::Error => Worker,
    belay_cron::CronError => Cron,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn usage_errors_exit_with_two_and_others_with_one() {
        assert_eq!(Error::Usage("bad flag".into()).exit_code(), 2);
        assert_eq!(Error::Invalid("ran dry".into()).exit_code(), 1);
    }

    #[test]
    fn io_errors_name_the_context_and_keep_the_source() {
        let e = Error::io(
            "creating a directory under /nope",
            io::Error::new(io::ErrorKind::PermissionDenied, "denied"),
        );
        assert_eq!(e.to_string(), "creating a directory under /nope: denied");
        assert!(e.source().is_some());
    }
}
