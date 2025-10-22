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

/// Framing of a request body.
pub(crate) fn request_framing(head: &RequestHead) -> Result<BodyFraming, ParseError> {
    let length = content_length(&head.headers)?;
    if !head.headers.contains(TRANSFER_ENCODING) {
        return Ok(match length {
            None | Some(0) => BodyFraming::Empty,
            Some(n) => BodyFraming::Length(n),
        });
    }
    if head.version == Version::Http10 {
        return Err(ParseError::InvalidTransferEncoding);
    }
    let codings: Vec<&str> = head.headers.tokens(TRANSFER_ENCODING).collect();
    match codings.last() {
        Some(last) if last.eq_ignore_ascii_case("chunked") => {
            if codings.len() == 1 {
                Ok(BodyFraming::Chunked)
            } else if codings[..codings.len() - 1]
                .iter()
                .any(|c| c.eq_ignore_ascii_case("chunked"))
            {
                // Chunked applied twice is never valid (RFC 9112 §6.1).
                Err(ParseError::InvalidTransferEncoding)
            } else {
                Err(ParseError::UnsupportedTransferEncoding(codings.join(", ")))
            }
        }
        _ => Err(ParseError::InvalidTransferEncoding),
    }
}

/// Framing of a response body, given the request method it answers.
pub(crate) fn response_framing(
    request_method: &Method,
    status: StatusCode,
    headers: &Headers,
) -> Result<BodyFraming, ParseError> {
    if *request_method == Method::Head || status.forbids_body() {
        return Ok(BodyFraming::Empty);
    }
    if headers.contains(TRANSFER_ENCODING) {
        let codings: Vec<&str> = headers.tokens(TRANSFER_ENCODING).collect();
        return match codings.last() {
            Some(last) if last.eq_ignore_ascii_case("chunked") => {
                if codings.len() == 1 {
                    Ok(BodyFraming::Chunked)
                } else {
                    Err(ParseError::UnsupportedTransferEncoding(codings.join(", ")))
                }
            }
            // A response may end a non-chunked coding by closing (§6.3 rule 4).
            _ => Ok(BodyFraming::UntilClose),
        };
    }
    Ok(match content_length(headers)? {
        Some(0) => BodyFraming::Empty,
        Some(n) => BodyFraming::Length(n),
        None => BodyFraming::UntilClose,
    })
}

/// Whether the sender of a message with these headers expects the
/// connection to stay open afterwards.
pub(crate) fn wants_keep_alive(version: Version, headers: &Headers) -> bool {
    if headers.has_token(CONNECTION, "close") {
        return false;
    }
    match version {
        Version::Http11 => true,
        Version::Http10 => headers.has_token(CONNECTION, "keep-alive"),
    }
}
