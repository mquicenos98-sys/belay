//! Errors from starting a server. Once it runs, every failure is answered
//! to the client as an [`ApiError`](belay_proto::api::ApiError) instead.

use std::fmt;
use std::io;

/// Why a server could not be started.
#[derive(Debug)]
pub enum Error {
    /// The listen address could not be bound.
    Bind {
        /// The address that was asked for, as written in `server.listen`.
        addr: String,
        /// The underlying error.
        source: io::Error,
    },
    /// The HTTP layer refused the listener or its settings, or a server
    /// thread could not be started.
    Http(belay_http::Error),
}

/// `Result` defaulting to this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Bind { addr, source } => write!(f, "cannot listen on {addr}: {source}"),
            Error::Http(e) => write!(f, "starting the HTTP server failed: {e}"),
        }
    }
}
