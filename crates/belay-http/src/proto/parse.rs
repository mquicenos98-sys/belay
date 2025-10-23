//! Incremental parsing of request and response heads (RFC 9112 §2–5).
//!
//! Parsing happens in two passes. [`HeadScanner`] is fed the growing input
//! buffer after every socket read and remembers how far it got, so a head
//! trickling in one byte at a time costs O(n) overall rather than O(n²). It
//! enforces the size limits *while* scanning, so an oversized head is
//! rejected the moment it crosses a limit instead of being buffered first,
//! and it validates the method token as it arrives, so a TLS handshake or
//! other binary garbage is refused on its first byte. Once the blank line
//! is found the complete head is parsed in one go.
//!
//! Syntax is strict where leniency would enable request smuggling
//! (whitespace before the colon, obsolete line folding, bare CR) and
//! lenient where RFC 9112 explicitly allows it (bare LF line endings,
//! empty lines before the request line).

use std::fmt;

use crate::headers::{is_field_byte, is_tchar, Headers};
use crate::method::{Method, Version};
use crate::status::StatusCode;

/// A violation of HTTP/1.1 message syntax or of a configured limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// The head exceeded the byte limit.
    HeadTooLarge {
        /// The limit in bytes.
        limit: usize,
    },
    /// The request line exceeded its byte limit.
    UriTooLong {
        /// The limit in bytes.
        limit: usize,
    },
    /// More header fields than allowed.
    TooManyHeaders {
        /// The limit.
        limit: usize,
    },
    /// The method is not a token.
    InvalidMethod,
    /// The request target is malformed or of an unsupported form.
    InvalidTarget,
    /// The request line is not `method SP target SP version`.
    InvalidRequestLine,
    /// The status line is not `version SP status [SP reason]`.
    InvalidStatusLine,
    /// The version is not `HTTP/<digit>.<digit>`.
    InvalidVersion,
    /// A well-formed version with a major number other than 1.
    UnsupportedVersion,
    /// A header field name that is not a token (including whitespace before
    /// the colon, RFC 9112 §5.1).
    InvalidHeaderName,
    /// A header field value with forbidden control characters.
    InvalidHeaderValue(String),
    /// A field line starting with whitespace (RFC 9112 §5.2).
    ObsoleteLineFolding,
    /// A CR not followed by LF.
    BareCarriageReturn,
    /// An HTTP/1.1 request without `Host` (RFC 9112 §3.2).
    MissingHost,
    /// More than one `Host` field.
    MultipleHost,
    /// A `Content-Length` that is not a number, or several that disagree.
    InvalidContentLength,
    /// Both `Transfer-Encoding` and `Content-Length`.
    ConflictingFraming,
    /// A transfer coding other than `chunked` (the string lists them).
    UnsupportedTransferEncoding(String),
    /// `Transfer-Encoding` whose final coding is not `chunked`, or one sent
    /// with HTTP/1.0.
    InvalidTransferEncoding,
    /// Malformed chunked framing; the string says what was wrong.
    InvalidChunk(&'static str),
    /// A body larger than the limit.
    BodyTooLarge {
        /// The limit in bytes.
        limit: u64,
    },
}

impl ParseError {
    /// The response status a server should answer this error with.
    pub fn status(&self) -> StatusCode {
        match self {
            ParseError::HeadTooLarge { .. } | ParseError::TooManyHeaders { .. } => {
                StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
            }
            ParseError::UriTooLong { .. } => StatusCode::URI_TOO_LONG,
            ParseError::UnsupportedVersion => StatusCode::HTTP_VERSION_NOT_SUPPORTED,
            ParseError::UnsupportedTransferEncoding(_) => StatusCode::NOT_IMPLEMENTED,
            ParseError::BodyTooLarge { .. } => StatusCode::CONTENT_TOO_LARGE,
            _ => StatusCode::BAD_REQUEST,
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::HeadTooLarge { limit } => {
                write!(f, "message head exceeds {limit} bytes")
            }
            ParseError::UriTooLong { limit } => write!(f, "request line exceeds {limit} bytes"),
            ParseError::TooManyHeaders { limit } => {
                write!(f, "more than {limit} header fields")
            }
            ParseError::InvalidMethod => f.write_str("method is not a valid token"),
            ParseError::InvalidTarget => f.write_str("invalid request target"),
            ParseError::InvalidRequestLine => f.write_str("malformed request line"),
            ParseError::InvalidStatusLine => f.write_str("malformed status line"),
            ParseError::InvalidVersion => f.write_str("malformed http version"),
            ParseError::UnsupportedVersion => f.write_str("only http/1.x is supported"),
            ParseError::InvalidHeaderName => f.write_str("invalid header field name"),
            ParseError::InvalidHeaderValue(name) => {
                write!(f, "invalid characters in value of header {name}")
            }
            ParseError::ObsoleteLineFolding => f.write_str("obsolete line folding is not accepted"),
            ParseError::BareCarriageReturn => f.write_str("carriage return without line feed"),
            ParseError::MissingHost => f.write_str("http/1.1 request without a host header"),
            ParseError::MultipleHost => f.write_str("more than one host header"),
            ParseError::InvalidContentLength => f.write_str("invalid content-length"),
            ParseError::ConflictingFraming => {
                f.write_str("both transfer-encoding and content-length are present")
            }
            ParseError::UnsupportedTransferEncoding(codings) => {
                write!(f, "unsupported transfer-encoding: {codings}")
            }
            ParseError::InvalidTransferEncoding => {
                f.write_str("transfer-encoding must end with chunked on http/1.1")
            }
            ParseError::InvalidChunk(what) => write!(f, "invalid chunked body: {what}"),
            ParseError::BodyTooLarge { limit } => write!(f, "body exceeds {limit} bytes"),
        }
    }
}
