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
    if length.is_some() {
        return Err(ParseError::ConflictingFraming);
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

/// RFC 9112 §3.2: an HTTP/1.1 request must carry exactly one `Host`.
pub(crate) fn check_host(head: &RequestHead) -> Result<(), ParseError> {
    match head.headers.get_all(HOST).count() {
        0 if head.version == Version::Http11 => Err(ParseError::MissingHost),
        0 | 1 => Ok(()),
        _ => Err(ParseError::MultipleHost),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(version: Version, headers: &[(&str, &str)]) -> RequestHead {
        RequestHead {
            method: Method::Post,
            target: "/".into(),
            version,
            headers: headers.iter().copied().collect(),
        }
    }

    fn framing(headers: &[(&str, &str)]) -> Result<BodyFraming, ParseError> {
        request_framing(&head(Version::Http11, headers))
    }

    #[test]
    fn content_length_variants() {
        assert_eq!(framing(&[]), Ok(BodyFraming::Empty));
        assert_eq!(framing(&[("Content-Length", "0")]), Ok(BodyFraming::Empty));
        assert_eq!(
            framing(&[("Content-Length", "42")]),
            Ok(BodyFraming::Length(42))
        );
        assert_eq!(
            framing(&[("Content-Length", "7"), ("content-length", "7, 7")]),
            Ok(BodyFraming::Length(7))
        );
    }

    #[test]
    fn rejects_bad_content_length() {
        for value in [
            "",
            "-1",
            "+5",
            "0x10",
            "1 2",
            "5, 6",
            "99999999999999999999999",
        ] {
            assert_eq!(
                framing(&[("Content-Length", value)]),
                Err(ParseError::InvalidContentLength),
                "{value:?}"
            );
        }
        assert_eq!(
            framing(&[("Content-Length", "5"), ("Content-Length", "6")]),
            Err(ParseError::InvalidContentLength)
        );
    }

    #[test]
    fn chunked_is_recognised_case_insensitively() {
        assert_eq!(
            framing(&[("Transfer-Encoding", "Chunked")]),
            Ok(BodyFraming::Chunked)
        );
    }

    #[test]
    fn transfer_encoding_with_content_length_is_refused() {
        // Regression: accepting this pair is the classic CL.TE smuggling vector.
        assert_eq!(
            framing(&[("Content-Length", "4"), ("Transfer-Encoding", "chunked")]),
            Err(ParseError::ConflictingFraming)
        );
    }

    #[test]
    fn unsupported_and_invalid_codings() {
        let err = framing(&[("Transfer-Encoding", "gzip, chunked")]).unwrap_err();
        assert_eq!(err.status(), StatusCode::NOT_IMPLEMENTED);
        assert_eq!(
            framing(&[("Transfer-Encoding", "gzip")]),
            Err(ParseError::InvalidTransferEncoding)
        );
        assert_eq!(
            framing(&[
                ("Transfer-Encoding", "chunked"),
                ("Transfer-Encoding", "chunked")
            ]),
            Err(ParseError::InvalidTransferEncoding)
        );
        assert_eq!(
            request_framing(&head(Version::Http10, &[("Transfer-Encoding", "chunked")])),
            Err(ParseError::InvalidTransferEncoding)
        );
    }

    #[test]
    fn response_framing_rules() {
        let none = Headers::new();
        let cl: Headers = [("Content-Length", "3")].into_iter().collect();
        let te: Headers = [("Transfer-Encoding", "chunked")].into_iter().collect();
        let gzip: Headers = [("Transfer-Encoding", "gzip")].into_iter().collect();
        let ok = StatusCode::OK;
        assert_eq!(
            response_framing(&Method::Head, ok, &cl),
            Ok(BodyFraming::Empty)
        );
        assert_eq!(
            response_framing(&Method::Get, StatusCode::NO_CONTENT, &cl),
            Ok(BodyFraming::Empty)
        );
        assert_eq!(
            response_framing(&Method::Get, StatusCode::NOT_MODIFIED, &te),
            Ok(BodyFraming::Empty)
        );
        assert_eq!(
            response_framing(&Method::Get, ok, &cl),
            Ok(BodyFraming::Length(3))
        );
        assert_eq!(
            response_framing(&Method::Get, ok, &te),
            Ok(BodyFraming::Chunked)
        );
        assert_eq!(
            response_framing(&Method::Get, ok, &gzip),
            Ok(BodyFraming::UntilClose)
        );
        assert_eq!(
            response_framing(&Method::Get, ok, &none),
            Ok(BodyFraming::UntilClose)
        );
    }

    #[test]
    fn keep_alive_defaults_depend_on_version() {
        let none = Headers::new();
        let close: Headers = [("Connection", "Close")].into_iter().collect();
        let keep: Headers = [("Connection", "keep-alive")].into_iter().collect();
        assert!(wants_keep_alive(Version::Http11, &none));
        assert!(!wants_keep_alive(Version::Http11, &close));
        assert!(!wants_keep_alive(Version::Http10, &none));
        assert!(wants_keep_alive(Version::Http10, &keep));
    }

    #[test]
    fn host_rules() {
        assert_eq!(
            check_host(&head(Version::Http11, &[])),
            Err(ParseError::MissingHost)
        );
        assert_eq!(check_host(&head(Version::Http10, &[])), Ok(()));
        assert_eq!(check_host(&head(Version::Http11, &[("Host", "a")])), Ok(()));
        assert_eq!(
            check_host(&head(Version::Http11, &[("Host", "a"), ("host", "b")])),
            Err(ParseError::MultipleHost)
        );
    }
}
