//! The crate-wide error type.
//!
//! Server-side protocol violations never surface as an [`Error`]: a
//! malformed request is answered with the matching 4xx status and the
//! connection is closed. This type covers the client, server setup, and the
//! parsing helpers that are useful on their own.

use std::fmt;
use std::io;

use crate::proto::ParseError;
use crate::status::StatusCode;
use crate::url::UrlError;

/// Errors produced by this crate.
#[derive(Debug)]
pub enum Error {
    /// A socket-level failure.
    Io(io::Error),
    /// A deadline elapsed; the string names the phase (`connect`, `read`,
    /// `write`).
    Timeout(&'static str),
    /// The listening socket could not be bound.
    Bind {
        /// The address that was requested.
        addr: String,
        /// The underlying error.
        source: io::Error,
    },
    /// A URL failed to parse.
    Url(UrlError),
    /// The URL scheme is not `http` (TLS is not supported).
    UnsupportedScheme(String),
    /// A method name that is not a valid token.
    InvalidMethod(String),
    /// A status code outside `100..=999`.
    InvalidStatus(u16),
    /// A header name or value that cannot be written to the wire.
    InvalidHeader(String),
    /// The peer sent a message that violates HTTP/1.1 syntax or framing.
    Protocol(ParseError),
    /// The peer closed the connection before a complete message arrived.
    ConnectionClosed,
    /// A body exceeded the configured size limit.
    BodyTooLarge {
        /// The limit in bytes.
        limit: u64,
    },
    /// A body that should be UTF-8 text is not.
    Utf8(std::str::Utf8Error),
    /// Redirects were followed until the policy's hop limit was reached.
    TooManyRedirects {
        /// The configured maximum.
        max: u32,
    },
    /// The server answered with a status the caller treats as a failure.
    Status {
        /// The response status.
        status: StatusCode,
        /// The start of the response body, decoded lossily.
        body: String,
    },
    /// A response carried a content type the caller cannot process.
    UnexpectedContentType {
        /// What the caller needed.
        expected: &'static str,
        /// What the server sent (empty when the header was missing).
        found: String,
    },
    /// An invalid configuration value.
    Config(String),
}

/// `Result` with this crate's [`Error`] as the default error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// Returns `true` for failures worth retrying on a fresh connection:
    /// socket errors, timeouts and connections closed mid-message.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Error::Io(_) | Error::Timeout(_) | Error::ConnectionClosed
        )
    }

    /// Returns `true` if a deadline elapsed.
    pub fn is_timeout(&self) -> bool {
        matches!(self, Error::Timeout(_))
    }

    /// Maps an I/O error from a socket operation, turning the platform's
    /// timeout kinds (`WouldBlock` on Unix, `TimedOut` on Windows) into
    /// [`Error::Timeout`].
    pub(crate) fn from_io(err: io::Error, phase: &'static str) -> Error {
        match err.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => Error::Timeout(phase),
            io::ErrorKind::UnexpectedEof => Error::ConnectionClosed,
            _ => Error::Io(err),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "i/o error: {e}"),
            Error::Timeout(phase) => write!(f, "{phase} timed out"),
            Error::Bind { addr, source } => write!(f, "cannot listen on {addr}: {source}"),
            Error::Url(e) => write!(f, "invalid url: {e}"),
            Error::UnsupportedScheme(s) => {
                write!(f, "unsupported url scheme `{s}` (only http is supported)")
            }
            Error::InvalidMethod(m) => write!(f, "invalid method {m:?}"),
            Error::InvalidStatus(code) => {
                write!(f, "invalid status code {code}, expected 100-999")
            }
            Error::InvalidHeader(msg) => f.write_str(msg),
            Error::Protocol(e) => write!(f, "malformed http message: {e}"),
            Error::ConnectionClosed => {
                f.write_str("connection closed before a complete message was received")
            }
            Error::BodyTooLarge { limit } => write!(f, "body exceeds the {limit} byte limit"),
            Error::Utf8(e) => write!(f, "body is not valid utf-8: {e}"),
            Error::TooManyRedirects { max } => write!(f, "gave up after {max} redirects"),
            Error::Status { status, body } => {
                if body.is_empty() {
                    write!(f, "server responded {status}")
                } else {
                    write!(f, "server responded {status}: {body}")
                }
            }
            Error::UnexpectedContentType { expected, found } => {
                if found.is_empty() {
                    write!(f, "expected content-type {expected}, got none")
                } else {
                    write!(f, "expected content-type {expected}, got {found}")
                }
            }
            Error::Config(msg) => write!(f, "invalid configuration: {msg}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) | Error::Bind { source: e, .. } => Some(e),
            Error::Url(e) => Some(e),
            Error::Protocol(e) => Some(e),
            Error::Utf8(e) => Some(e),
            _ => None,
        }
    }
}
