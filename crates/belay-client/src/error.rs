//! What can go wrong talking to a belay server, and how to tell the cases
//! apart: the server said no ([`Error::Api`]), the conversation failed
//! ([`Error::Transport`]), the answer made no sense ([`Error::Decode`]), or
//! the client was set up wrongly ([`Error::Config`]).

use std::fmt;
use std::io;

use belay_proto::api::{ApiError, ErrorCode};

/// Errors returned by the client.
#[derive(Debug)]
pub enum Error {
    /// The server answered with an error, decoded from its body (or
    /// reconstructed from the status when there was no body, e.g. from a
    /// proxy). Branch on [`ApiError::code`], never on the message.
    Api(ApiError),
    /// The request did not get an answer: connection refused or reset, a
    /// timeout, a malformed response, too many redirects.
    Transport(belay_http::Error),
    /// The server answered successfully, but the body is not what this
    /// endpoint returns. Usually a version mismatch or a proxy answering
    /// in place of belay.
    Decode(DecodeError),
    /// The client configuration is unusable (bad base URL, bad token).
    Config(String),
}

/// `Result` with the client's [`Error`] as the default error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// The server's error, if it answered with one.
    #[must_use]
    pub fn api_error(&self) -> Option<&ApiError> {
        match self {
            Error::Api(e) => Some(e),
            _ => None,
        }
    }

    /// The server's error code, if it answered with an error.
    #[must_use]
    pub fn code(&self) -> Option<ErrorCode> {
        self.api_error().map(|e| e.code)
    }

    /// Whether the server said the resource does not exist.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        self.code() == Some(ErrorCode::NotFound)
    }

    /// Whether a worker's lease is gone (expired or handed to another
    /// worker); the job must be abandoned without further reports.
    #[must_use]
    pub fn is_lease_lost(&self) -> bool {
        self.code() == Some(ErrorCode::LeaseLost)
    }

    /// Whether the same call may succeed later: the server was unavailable
    /// or rate limited, or the connection failed or timed out. The client
    /// has already retried what it safely could by the time this is seen;
    /// retrying a non-idempotent call is the caller's decision.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Api(e) => e.is_retryable(),
            Error::Transport(e) => e.is_transient(),
            Error::Decode(_) | Error::Config(_) => false,
        }
    }

    /// Whether the request timed out waiting for the server.
    #[must_use]
    pub fn is_timeout(&self) -> bool {
        matches!(self, Error::Transport(e) if e.is_timeout())
    }

    /// Converts to the [`ApiError`] shape used by
    /// [`JobSource`](belay_proto::JobSource), so a worker handles remote
    /// and embedded sources alike. Connection failures and timeouts become
    /// `unavailable` (retryable), undecodable answers `internal`.
    #[must_use]
    pub fn into_api_error(self) -> ApiError {
        match self {
            Error::Api(e) => e,
            Error::Transport(e) if e.is_transient() => {
                ApiError::unavailable(format!("belay server unreachable: {e}"))
            }
            Error::Transport(e) => ApiError::internal(format!("request failed: {e}")),
            Error::Decode(e) => ApiError::internal(e.to_string()),
            Error::Config(message) => ApiError::invalid_argument(message),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Api(e) => write!(f, "server error: {e}"),
            Error::Transport(e) => write!(f, "request failed: {e}"),
            Error::Decode(e) => e.fmt(f),
            Error::Config(message) => write!(f, "invalid client configuration: {message}"),
        }
    }
}
