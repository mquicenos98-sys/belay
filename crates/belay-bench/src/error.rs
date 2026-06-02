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
