//! Serialization of request and response heads.
//!
//! Framing fields (`Content-Length`, `Transfer-Encoding`, `Connection`,
//! `Keep-Alive`) are owned by the writer: whatever the caller put in its
//! header map for them is dropped and replaced by values derived from the
//! actual body, so a handler cannot accidentally desynchronise the stream.

use crate::error::Error;
use crate::headers::names::{
    CONNECTION, CONTENT_LENGTH, DATE, HOST, KEEP_ALIVE, SERVER, TRANSFER_ENCODING, USER_AGENT,
};
use crate::headers::{is_valid_name, Headers};
use crate::method::Method;
use crate::status::StatusCode;

/// How a response body will be delimited on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutFraming {
    /// No framing field at all (204, 1xx, or 304 without a known length).
    None,
    /// `Content-Length: n`.
    Length(u64),
    /// `Transfer-Encoding: chunked`.
    Chunked,
    /// Delimited by closing the connection (streams to HTTP/1.0 clients).
    Close,
}

/// Everything about a response head that is not in the handler's
/// [`Headers`].
#[derive(Debug)]
pub(crate) struct ResponseMeta<'a> {
    pub framing: OutFraming,
    /// Value for the `Connection` field, if one should be sent.
    pub connection: Option<&'static str>,
    pub date: Option<&'a str>,
    pub server: Option<&'a str>,
}

pub(crate) fn push_field(out: &mut Vec<u8>, name: &str, value: &str) -> Result<(), Error> {
    if !is_valid_name(name) {
        return Err(Error::InvalidHeader(format!(
            "invalid header name {name:?}"
        )));
    }
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(b": ");
    out.extend_from_slice(value.as_bytes());
    out.extend_from_slice(b"\r\n");
    Ok(())
}

fn is_framing_field(name: &str) -> bool {
    [CONTENT_LENGTH, TRANSFER_ENCODING, CONNECTION, KEEP_ALIVE]
        .iter()
        .any(|f| name.eq_ignore_ascii_case(f))
}

/// Writes a status line and header section. Responses always claim
/// HTTP/1.1, the highest version this server conforms to (RFC 9110 §6.2).
pub(crate) fn encode_response_head(
    out: &mut Vec<u8>,
    status: StatusCode,
    headers: &Headers,
    meta: &ResponseMeta<'_>,
) -> Result<(), Error> {
    out.extend_from_slice(b"HTTP/1.1 ");
    out.extend_from_slice(status.as_u16().to_string().as_bytes());
    // The space before the reason phrase is required even when it is empty.
    out.push(b' ');
    out.extend_from_slice(status.canonical_reason().unwrap_or("").as_bytes());
    out.extend_from_slice(b"\r\n");
    if let Some(date) = meta.date {
        if !headers.contains(DATE) {
            push_field(out, DATE, date)?;
        }
    }
    if let Some(server) = meta.server {
        if !headers.contains(SERVER) {
            push_field(out, SERVER, server)?;
        }
    }
    for (name, value) in headers.iter() {
        if !is_framing_field(name) {
            push_field(out, name, value)?;
        }
    }
    match meta.framing {
        OutFraming::Length(n) => push_field(out, CONTENT_LENGTH, &n.to_string())?,
        OutFraming::Chunked => push_field(out, TRANSFER_ENCODING, "chunked")?,
        OutFraming::None | OutFraming::Close => {}
    }
    if let Some(connection) = meta.connection {
        push_field(out, CONNECTION, connection)?;
    }
    out.extend_from_slice(b"\r\n");
    Ok(())
}

/// Writes a request line and header section for a body of `body_len`
/// bytes. `Host` and `User-Agent` are added unless the caller set them.
pub(crate) fn encode_request_head(
    out: &mut Vec<u8>,
    method: &Method,
    target: &str,
    host: &str,
    user_agent: &str,
    headers: &Headers,
    body_len: usize,
) -> Result<(), Error> {
    if Method::from_token(method.as_str().as_bytes()).is_none() {
        return Err(Error::InvalidMethod(method.as_str().to_owned()));
    }
    if target.is_empty() || !target.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err(Error::InvalidHeader(format!(
            "invalid request target {target:?}"
        )));
    }
    out.extend_from_slice(method.as_str().as_bytes());
    out.push(b' ');
    out.extend_from_slice(target.as_bytes());
    out.extend_from_slice(b" HTTP/1.1\r\n");
    if !headers.contains(HOST) {
        push_field(out, HOST, host)?;
    }
    if !user_agent.is_empty() && !headers.contains(USER_AGENT) {
        push_field(out, USER_AGENT, user_agent)?;
    }
    for (name, value) in headers.iter() {
        let framing = name.eq_ignore_ascii_case(CONTENT_LENGTH)
            || name.eq_ignore_ascii_case(TRANSFER_ENCODING);
        if !framing {
            push_field(out, name, value)?;
        }
    }
    // RFC 9110 §8.6: send a length for empty bodies of methods that
    // normally carry one, so the server does not wait for a body.
    if body_len > 0 || matches!(method, Method::Post | Method::Put | Method::Patch) {
        push_field(out, CONTENT_LENGTH, &body_len.to_string())?;
    }
    out.extend_from_slice(b"\r\n");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(headers: &[(&str, &str)], meta: ResponseMeta<'_>) -> Result<String, Error> {
        let headers: Headers = headers.iter().copied().collect();
        let mut out = Vec::new();
        encode_response_head(&mut out, StatusCode::OK, &headers, &meta)?;
        Ok(String::from_utf8(out).expect("heads are ascii"))
    }

    fn meta(framing: OutFraming) -> ResponseMeta<'static> {
        ResponseMeta {
            framing,
            connection: None,
            date: None,
            server: None,
        }
    }

    #[test]
    fn writes_status_line_and_length() {
        let head = response(
            &[("Content-Type", "text/plain")],
            meta(OutFraming::Length(5)),
        )
        .expect("valid head");
        assert_eq!(
            head,
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\n"
        );
    }

    #[test]
    fn handler_framing_fields_are_replaced() {
        let head = response(
            &[
                ("Content-Length", "999"),
                ("transfer-encoding", "gzip"),
                ("Connection", "upgrade"),
                ("X-Keep", "yes"),
            ],
            ResponseMeta {
                connection: Some("close"),
                ..meta(OutFraming::Chunked)
            },
        )
        .expect("valid head");
        assert_eq!(
            head,
            "HTTP/1.1 200 OK\r\nX-Keep: yes\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
        );
    }

    #[test]
    fn date_and_server_do_not_override_handler_values() {
        let head = response(
            &[("Server", "custom")],
            ResponseMeta {
                date: Some("Thu, 01 Jan 1970 00:00:00 GMT"),
                server: Some("belay"),
                ..meta(OutFraming::None)
            },
        )
        .expect("valid head");
        assert!(head.contains("Date: Thu, 01 Jan 1970 00:00:00 GMT\r\n"));
        assert!(head.contains("Server: custom\r\n"));
        assert!(!head.contains("belay"));
    }

    #[test]
    fn unknown_status_keeps_the_separator_space() {
        let mut out = Vec::new();
        let status = StatusCode::from_u16(599).expect("in range");
        encode_response_head(&mut out, status, &Headers::new(), &meta(OutFraming::None))
            .expect("valid head");
        assert!(out.starts_with(b"HTTP/1.1 599 \r\n"));
    }

    #[test]
    fn request_head_adds_host_agent_and_length() {
        let mut out = Vec::new();
        let headers: Headers = [("Accept", "application/json"), ("content-length", "1")]
            .into_iter()
            .collect();
        encode_request_head(
            &mut out,
            &Method::Post,
            "/v1/jobs",
            "h:8080",
            "t/1",
            &headers,
            0,
        )
        .expect("valid head");
        assert_eq!(
            String::from_utf8(out).expect("ascii"),
            "POST /v1/jobs HTTP/1.1\r\nHost: h:8080\r\nUser-Agent: t/1\r\nAccept: application/json\r\nContent-Length: 0\r\n\r\n"
        );
    }

    #[test]
    fn request_head_respects_caller_host_and_omits_length_for_get() {
        let mut out = Vec::new();
        let headers: Headers = [("Host", "virtual")].into_iter().collect();
        encode_request_head(&mut out, &Method::Get, "/", "real", "", &headers, 0)
            .expect("valid head");
        assert_eq!(
            String::from_utf8(out).expect("ascii"),
            "GET / HTTP/1.1\r\nHost: virtual\r\n\r\n"
        );
    }

    #[test]
    fn request_head_rejects_invalid_methods_and_targets() {
        let mut out = Vec::new();
        let bad = Method::Extension("NOT VALID".into());
        assert!(matches!(
            encode_request_head(&mut out, &bad, "/", "h", "", &Headers::new(), 0),
            Err(Error::InvalidMethod(_))
        ));
        assert!(
            encode_request_head(&mut out, &Method::Get, "/a b", "h", "", &Headers::new(), 0)
                .is_err()
        );
    }
}
