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
