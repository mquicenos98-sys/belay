//! Message body framing and connection persistence (RFC 9112 §6 and §9.3).
//!
//! These rules decide where one message ends and the next begins, which is
//! exactly where request smuggling lives, so the request side refuses every
//! ambiguous combination instead of picking an interpretation: both
//! `Transfer-Encoding` and `Content-Length`, disagreeing `Content-Length`
//! values, `Transfer-Encoding` on HTTP/1.0, and transfer codings that do not
//! end in `chunked`.

use crate::headers::names::{CONNECTION, CONTENT_LENGTH, HOST, TRANSFER_ENCODING};
use crate::headers::Headers;
use crate::method::{Method, Version};
use crate::proto::parse::{ParseError, RequestHead};
use crate::status::StatusCode;

/// How the body of a message is delimited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BodyFraming {
    /// No body.
    Empty,
    /// Exactly this many bytes.
    Length(u64),
    /// Chunked transfer coding.
    Chunked,
    /// Everything until the peer closes (responses only).
    UntilClose,
}

/// The combined `Content-Length`, if present. Repeated fields and
/// comma-separated lists are accepted only when every value agrees
/// (RFC 9112 §6.3 rule 5).
pub(crate) fn content_length(headers: &Headers) -> Result<Option<u64>, ParseError> {
    let mut found = None;
    for value in headers.get_all(CONTENT_LENGTH) {
        for part in value.split(',') {
            let part = part.trim_matches(|c| c == ' ' || c == '\t');
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(ParseError::InvalidContentLength);
            }
            let n: u64 = part.parse().map_err(|_| ParseError::InvalidContentLength)?;
            match found {
                Some(prev) if prev != n => return Err(ParseError::InvalidContentLength),
                _ => found = Some(n),
            }
        }
    }
    Ok(found)
}
