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

impl std::error::Error for ParseError {}

/// Size limits applied while scanning a head.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HeadLimits {
    /// Maximum bytes in the start line (without CRLF).
    pub start_line: usize,
    /// Maximum bytes in the whole head, including leading blank lines.
    pub head: usize,
    /// Maximum number of header field lines.
    pub headers: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartLine {
    Request,
    Response,
}

/// Finds the end of a head in a buffer that grows between calls.
#[derive(Debug)]
pub(crate) struct HeadScanner {
    kind: StartLine,
    limits: HeadLimits,
    /// Next byte to examine.
    pos: usize,
    line_start: usize,
    /// Where the start line begins (after skipped blank lines).
    head_start: usize,
    start_line_done: bool,
    /// Whether the method token of a request line has ended (seen SP).
    method_done: bool,
    header_lines: usize,
}

impl HeadScanner {
    pub(crate) fn new(kind: StartLine, limits: HeadLimits) -> Self {
        HeadScanner {
            kind,
            limits,
            pos: 0,
            line_start: 0,
            head_start: 0,
            start_line_done: false,
            method_done: false,
            header_lines: 0,
        }
    }

    /// Continues scanning `buf`, which must extend the buffer passed to the
    /// previous call. Returns `(start, end)` once the head is complete:
    /// `buf[start..end]` is the head including its blank line, and `end`
    /// bytes should be consumed.
    pub(crate) fn scan(&mut self, buf: &[u8]) -> Result<Option<(usize, usize)>, ParseError> {
        while self.pos < buf.len() {
            let b = buf[self.pos];
            self.pos += 1;
            if b != b'\n' {
                if self.kind == StartLine::Request && !self.start_line_done && !self.method_done {
                    if b == b' ' {
                        self.method_done = true;
                    } else if b != b'\r' && !is_tchar(b) {
                        return Err(ParseError::InvalidMethod);
                    }
                }
                continue;
            }
            let mut end = self.pos - 1;
            if end > self.line_start && buf[end - 1] == b'\r' {
                end -= 1;
            }
            if end == self.line_start {
                if !self.start_line_done {
                    // RFC 9112 §2.2: ignore empty lines before the start line.
                    self.line_start = self.pos;
                    self.head_start = self.pos;
                    continue;
                }
                let start = self.head_start;
                let end = self.pos;
                self.reset();
                return Ok(Some((start, end)));
            }
            if !self.start_line_done {
                if end - self.line_start > self.limits.start_line {
                    return Err(self.start_line_error());
                }
                self.start_line_done = true;
            } else {
                self.header_lines += 1;
                if self.header_lines > self.limits.headers {
                    return Err(ParseError::TooManyHeaders {
                        limit: self.limits.headers,
                    });
                }
            }
            self.line_start = self.pos;
        }
        if !self.start_line_done && buf.len() - self.head_start > self.limits.start_line {
            return Err(self.start_line_error());
        }
        if buf.len() > self.limits.head {
            return Err(ParseError::HeadTooLarge {
                limit: self.limits.head,
            });
        }
        Ok(None)
    }

    fn start_line_error(&self) -> ParseError {
        match self.kind {
            StartLine::Request => ParseError::UriTooLong {
                limit: self.limits.start_line,
            },
            StartLine::Response => ParseError::HeadTooLarge {
                limit: self.limits.start_line,
            },
        }
    }

    fn reset(&mut self) {
        *self = HeadScanner::new(self.kind, self.limits);
    }
}

/// A parsed request line plus header fields.
#[derive(Debug)]
pub(crate) struct RequestHead {
    pub method: Method,
    pub target: String,
    pub version: Version,
    pub headers: Headers,
}

/// A parsed status line plus header fields.
#[derive(Debug)]
pub(crate) struct ResponseHead {
    pub version: Version,
    pub status: StatusCode,
    pub headers: Headers,
}

/// Scans and parses request heads.
#[derive(Debug)]
pub(crate) struct RequestParser {
    scanner: HeadScanner,
}

impl RequestParser {
    pub(crate) fn new(limits: HeadLimits) -> Self {
        RequestParser {
            scanner: HeadScanner::new(StartLine::Request, limits),
        }
    }

    /// Returns the head and the number of bytes it occupied once complete.
    pub(crate) fn parse(&mut self, buf: &[u8]) -> Result<Option<(RequestHead, usize)>, ParseError> {
        match self.scanner.scan(buf)? {
            None => Ok(None),
            Some((start, end)) => Ok(Some((parse_request_head(&buf[start..end])?, end))),
        }
    }
}

/// Scans and parses response heads.
#[derive(Debug)]
pub(crate) struct ResponseParser {
    scanner: HeadScanner,
}

impl ResponseParser {
    pub(crate) fn new(limits: HeadLimits) -> Self {
        ResponseParser {
            scanner: HeadScanner::new(StartLine::Response, limits),
        }
    }

    pub(crate) fn parse(
        &mut self,
        buf: &[u8],
    ) -> Result<Option<(ResponseHead, usize)>, ParseError> {
        match self.scanner.scan(buf)? {
            None => Ok(None),
            Some((start, end)) => Ok(Some((parse_response_head(&buf[start..end])?, end))),
        }
    }
}

/// Splits a complete head into lines without their terminators, rejecting
/// bare CRs, and stops at the blank line.
fn lines(head: &[u8]) -> impl Iterator<Item = Result<&[u8], ParseError>> {
    head.split(|&b| b == b'\n')
        .map(|line| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.contains(&b'\r') {
                Err(ParseError::BareCarriageReturn)
            } else {
                Ok(line)
            }
        })
        .take_while(|line| !matches!(line, Ok(l) if l.is_empty()))
}

fn parse_request_head(head: &[u8]) -> Result<RequestHead, ParseError> {
    let mut lines = lines(head);
    let first = lines.next().ok_or(ParseError::InvalidRequestLine)??;
    let (method, target, version) = parse_request_line(first)?;
    let headers = parse_fields(lines)?;
    Ok(RequestHead {
        method,
        target,
        version,
        headers,
    })
}

fn parse_request_line(line: &[u8]) -> Result<(Method, String, Version), ParseError> {
    let mut parts = line.splitn(3, |&b| b == b' ');
    let method = parts.next().ok_or(ParseError::InvalidRequestLine)?;
    let target = parts.next().ok_or(ParseError::InvalidRequestLine)?;
    let version = parts.next().ok_or(ParseError::InvalidRequestLine)?;
    let method = Method::from_token(method).ok_or(ParseError::InvalidMethod)?;
    if target.is_empty()
        || !target
            .iter()
            .all(|&b| (0x21..=0x7e).contains(&b) && b != b'#')
    {
        return Err(ParseError::InvalidTarget);
    }
    let version = parse_version(version)?;
    // Visible ASCII only, so the conversion is lossless.
    let target = String::from_utf8_lossy(target).into_owned();
    let form_ok = target.starts_with('/')
        || target == "*"
        || starts_with_ignore_case(&target, "http://")
        || starts_with_ignore_case(&target, "https://")
        || method == Method::Connect;
    if !form_ok {
        return Err(ParseError::InvalidTarget);
    }
    Ok((method, target, version))
}

fn parse_version(v: &[u8]) -> Result<Version, ParseError> {
    match v {
        b"HTTP/1.1" => Ok(Version::Http11),
        b"HTTP/1.0" => Ok(Version::Http10),
        [b'H', b'T', b'T', b'P', b'/', major, b'.', minor]
            if major.is_ascii_digit() && minor.is_ascii_digit() =>
        {
            // Minor versions are backwards compatible (RFC 9110 §2.5):
            // answer HTTP/1.2+ as 1.1.
            if *major == b'1' {
                Ok(Version::Http11)
            } else {
                Err(ParseError::UnsupportedVersion)
            }
        }
        _ => Err(ParseError::InvalidVersion),
    }
}

fn parse_response_head(head: &[u8]) -> Result<ResponseHead, ParseError> {
    let mut lines = lines(head);
    let first = lines.next().ok_or(ParseError::InvalidStatusLine)??;
    if first.len() < 12 || first[8] != b' ' {
        return Err(ParseError::InvalidStatusLine);
    }
    let version = parse_version(&first[..8]).map_err(|e| match e {
        ParseError::UnsupportedVersion => e,
        _ => ParseError::InvalidStatusLine,
    })?;
    let code = &first[9..12];
    if !code.iter().all(u8::is_ascii_digit) || (first.len() > 12 && first[12] != b' ') {
        return Err(ParseError::InvalidStatusLine);
    }
    let code = code
        .iter()
        .fold(0u16, |acc, &d| acc * 10 + u16::from(d - b'0'));
    let status = StatusCode::from_u16(code).map_err(|_| ParseError::InvalidStatusLine)?;
    let headers = parse_fields(lines)?;
    Ok(ResponseHead {
        version,
        status,
        headers,
    })
}

fn parse_fields<'a>(
    lines: impl Iterator<Item = Result<&'a [u8], ParseError>>,
) -> Result<Headers, ParseError> {
    let mut headers = Headers::new();
    for line in lines {
        let line = line?;
        if line[0] == b' ' || line[0] == b'\t' {
            return Err(ParseError::ObsoleteLineFolding);
        }
        let colon = line
            .iter()
            .position(|&b| b == b':')
            .ok_or(ParseError::InvalidHeaderName)?;
        let name = &line[..colon];
        if name.is_empty() || !name.iter().all(|&b| is_tchar(b)) {
            return Err(ParseError::InvalidHeaderName);
        }
        let name = String::from_utf8_lossy(name).into_owned();
        let value = trim_ows(&line[colon + 1..]);
        if !value.iter().all(|&b| is_field_byte(b)) {
            return Err(ParseError::InvalidHeaderValue(name));
        }
        headers.append(name, field_value_to_string(value));
    }
    Ok(headers)
}

fn trim_ows(mut v: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = v {
        v = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = v {
        v = rest;
    }
    v
}

/// Field values are UTF-8 in practice; anything else is taken as
/// ISO-8859-1, the historical charset (RFC 9110 §5.5), so no byte is lost.
fn field_value_to_string(v: &[u8]) -> String {
    match std::str::from_utf8(v) {
        Ok(s) => s.to_owned(),
        Err(_) => v.iter().map(|&b| char::from(b)).collect(),
    }
}

pub(crate) fn starts_with_ignore_case(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: HeadLimits = HeadLimits {
        start_line: 64,
        head: 256,
        headers: 4,
    };

    fn parse(input: &[u8]) -> Result<Option<(RequestHead, usize)>, ParseError> {
        RequestParser::new(LIMITS).parse(input)
    }

    fn complete(input: &[u8]) -> RequestHead {
        match parse(input) {
            Ok(Some((head, len))) => {
                assert_eq!(len, input.len(), "whole input should be the head");
                head
            }
            other => panic!("expected a complete head, got {other:?}"),
        }
    }

    #[test]
    fn parses_a_simple_request() {
        let head = complete(
            b"GET /v1/jobs?state=ready HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\n\r\n",
        );
        assert_eq!(head.method, Method::Get);
        assert_eq!(head.target, "/v1/jobs?state=ready");
        assert_eq!(head.version, Version::Http11);
        assert_eq!(head.headers.get("host"), Some("localhost"));
        assert_eq!(head.headers.len(), 2);
    }

    #[test]
    fn byte_by_byte_feeding_finds_the_same_head() {
        let input = b"\r\nPOST /x HTTP/1.1\r\nHost: h\r\nContent-Length: 3\r\n\r\nabc";
        let mut parser = RequestParser::new(LIMITS);
        let mut result = None;
        for end in 1..=input.len() {
            if let Some(done) = parser.parse(&input[..end]).expect("valid prefix") {
                result = Some((done, end));
                break;
            }
        }
        let ((head, consumed), fed) = result.expect("head completes");
        assert_eq!(
            consumed,
            input.len() - 3,
            "body bytes are not part of the head"
        );
        assert_eq!(fed, consumed);
        assert_eq!(head.method, Method::Post);
        assert_eq!(head.headers.get("content-length"), Some("3"));
    }

    #[test]
    fn accepts_bare_lf_line_endings() {
        let head = complete(b"GET / HTTP/1.0\nX-A: 1\n\n");
        assert_eq!(head.version, Version::Http10);
        assert_eq!(head.headers.get("x-a"), Some("1"));
    }

    #[test]
    fn trims_optional_whitespace_around_values() {
        let head = complete(b"GET / HTTP/1.1\r\nHost:\t example \t\r\nEmpty:\r\n\r\n");
        assert_eq!(head.headers.get("host"), Some("example"));
        assert_eq!(head.headers.get("empty"), Some(""));
    }

    #[test]
    fn rejects_binary_garbage_on_the_first_byte() {
        // A TLS ClientHello starts with 0x16 0x03.
        assert_eq!(parse(b"\x16").unwrap_err(), ParseError::InvalidMethod);
        assert_eq!(parse(b"GE(").unwrap_err(), ParseError::InvalidMethod);
    }

    #[test]
    fn long_request_line_is_rejected_before_it_completes() {
        let mut input = b"GET /".to_vec();
        input.extend(std::iter::repeat(b'a').take(70));
        assert_eq!(
            parse(&input).unwrap_err(),
            ParseError::UriTooLong { limit: 64 }
        );
        assert_eq!(
            parse(&input).unwrap_err().status(),
            StatusCode::URI_TOO_LONG
        );
    }

    #[test]
    fn oversized_head_is_rejected_before_it_completes() {
        let mut input = b"GET / HTTP/1.1\r\nX-Big: ".to_vec();
        input.extend(std::iter::repeat(b'v').take(300));
        let err = parse(&input).unwrap_err();
        assert_eq!(err, ParseError::HeadTooLarge { limit: 256 });
        assert_eq!(err.status(), StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE);
    }

    #[test]
    fn too_many_header_lines() {
        let input = b"GET / HTTP/1.1\r\nA: 1\r\nB: 2\r\nC: 3\r\nD: 4\r\nE: 5\r\n";
        assert_eq!(
            parse(input).unwrap_err(),
            ParseError::TooManyHeaders { limit: 4 }
        );
    }

    #[test]
    fn endless_blank_lines_hit_the_head_limit() {
        let input = b"\r\n".repeat(200);
        assert_eq!(
            parse(&input).unwrap_err(),
            ParseError::HeadTooLarge { limit: 256 }
        );
    }

    #[test]
    fn rejects_smuggling_prone_syntax() {
        let cases: [(&[u8], ParseError); 5] = [
            (
                b"GET / HTTP/1.1\r\nHost : h\r\n\r\n",
                ParseError::InvalidHeaderName,
            ),
            (
                b"GET / HTTP/1.1\r\nX: a\r\n folded\r\n\r\n",
                ParseError::ObsoleteLineFolding,
            ),
            (
                b"GET / HTTP/1.1\r\nX: a\rb\r\n\r\n",
                ParseError::BareCarriageReturn,
            ),
            (
                b"GET / HTTP/1.1\r\nX: a\x00b\r\n\r\n",
                ParseError::InvalidHeaderValue("X".into()),
            ),
            (
                b"GET / HTTP/1.1\r\nNoColon\r\n\r\n",
                ParseError::InvalidHeaderName,
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(
                parse(input).unwrap_err(),
                expected,
                "{:?}",
                String::from_utf8_lossy(input)
            );
        }
    }

    #[test]
    fn request_line_shape_is_strict() {
        let cases: [(&[u8], ParseError); 7] = [
            (b"GET  / HTTP/1.1\r\n\r\n", ParseError::InvalidTarget),
            (b"GET / HTTP/1.1 \r\n\r\n", ParseError::InvalidVersion),
            (b"GET /\r\n\r\n", ParseError::InvalidRequestLine),
            (b"GET /a#frag HTTP/1.1\r\n\r\n", ParseError::InvalidTarget),
            (b"GET relative HTTP/1.1\r\n\r\n", ParseError::InvalidTarget),
            (b"GET / HTTP/2.0\r\n\r\n", ParseError::UnsupportedVersion),
            (b"GET / HTTPS/1.1\r\n\r\n", ParseError::InvalidVersion),
        ];
        for (input, expected) in cases {
            assert_eq!(
                parse(input).unwrap_err(),
                expected,
                "{:?}",
                String::from_utf8_lossy(input)
            );
        }
    }

    #[test]
    fn accepts_absolute_and_asterisk_targets() {
        assert_eq!(
            complete(b"GET http://h/x HTTP/1.1\r\n\r\n").target,
            "http://h/x"
        );
        assert_eq!(complete(b"OPTIONS * HTTP/1.1\r\n\r\n").target, "*");
        assert_eq!(complete(b"GET / HTTP/1.7\r\n\r\n").version, Version::Http11);
    }

    #[test]
    fn non_utf8_values_are_read_as_latin1() {
        let head = complete(b"GET / HTTP/1.1\r\nX-Name: caf\xe9\r\n\r\n");
        assert_eq!(head.headers.get("x-name"), Some("caf\u{e9}"));
    }

    #[test]
    fn pipelined_input_stops_at_the_first_head() {
        let input = b"GET /a HTTP/1.1\r\n\r\nGET /b HTTP/1.1\r\n\r\n";
        let (head, len) = parse(input).expect("valid").expect("complete");
        assert_eq!(head.target, "/a");
        let (second, _) = parse(&input[len..]).expect("valid").expect("complete");
        assert_eq!(second.target, "/b");
    }

    #[test]
    fn parses_status_lines() {
        let mut parser = ResponseParser::new(LIMITS);
        let (head, _) = parser
            .parse(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
            .expect("valid")
            .expect("complete");
        assert_eq!(head.status, StatusCode::NOT_FOUND);
        assert_eq!(head.version, Version::Http11);

        let (head, _) = ResponseParser::new(LIMITS)
            .parse(b"HTTP/1.0 299\r\n\r\n")
            .expect("reason is optional")
            .expect("complete");
        assert_eq!(head.status, 299);
        assert_eq!(head.version, Version::Http10);
    }

    #[test]
    fn rejects_malformed_status_lines() {
        for bad in [
            &b"HTTP/1.1 20 OK\r\n\r\n"[..],
            b"HTTP/1.1 2000 OK\r\n\r\n",
            b"HTTP/1.1 abc OK\r\n\r\n",
            b"ICY 200 OK\r\n\r\n",
            b"HTTP/1.1 099 Low\r\n\r\n",
        ] {
            let result = ResponseParser::new(LIMITS).parse(bad);
            assert_eq!(
                result.unwrap_err(),
                ParseError::InvalidStatusLine,
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
        let result = ResponseParser::new(LIMITS).parse(b"HTTP/2.0 200 OK\r\n\r\n");
        assert_eq!(result.unwrap_err(), ParseError::UnsupportedVersion);
    }
}
