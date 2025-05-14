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
