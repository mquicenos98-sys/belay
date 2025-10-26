//! Server configuration.

use std::fmt;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::method::Method;
use crate::proto::parse::HeadLimits;

/// Problems the server cannot report to a client, passed to
/// [`ServerConfig::error_hook`] so the application can log them.
///
/// Malformed requests are not reported here: they are the client's
/// problem, answered with a 4xx, and counted in
/// [`ServerStats::protocol_errors`](crate::ServerStats::protocol_errors).
#[derive(Debug)]
pub enum ServerError {
    /// `accept` failed with something other than a per-connection error,
    /// typically file-descriptor exhaustion. The server backs off briefly
    /// and keeps accepting.
    Accept(io::Error),
    /// A connection thread could not be spawned; the client got a 503.
    Spawn(io::Error),
    /// A handler or streaming body panicked; the client got a 500 (or, if
    /// the body was already streaming, a closed connection).
    HandlerPanic {
        /// Request method.
        method: Method,
        /// Request path.
        path: String,
        /// The panic message.
        message: String,
    },
    /// A handler produced a response that cannot be sent (an invalid
    /// header, or a 1xx status); the client got a 500 instead.
    InvalidResponse {
        /// Request method.
        method: Method,
        /// Request path.
        path: String,
        /// What was wrong.
        reason: String,
    },
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServerError::Accept(e) => write!(f, "accept failed: {e}"),
            ServerError::Spawn(e) => write!(f, "cannot spawn connection thread: {e}"),
            ServerError::HandlerPanic {
                method,
                path,
                message,
            } => write!(f, "handler for {method} {path} panicked: {message}"),
            ServerError::InvalidResponse {
                method,
                path,
                reason,
            } => write!(f, "invalid response to {method} {path}: {reason}"),
        }
    }
}

impl std::error::Error for ServerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ServerError::Accept(e) | ServerError::Spawn(e) => Some(e),
            _ => None,
        }
    }
}

/// Callback for [`ServerError`]s.
pub type ErrorHook = Arc<dyn Fn(&ServerError) + Send + Sync>;
