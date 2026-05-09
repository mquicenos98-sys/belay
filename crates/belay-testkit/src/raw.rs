//! Raw HTTP/1.1 over `std::net`, for what the typed client hides on
//! purpose: redirects it follows, headers it consumes, and exactly which
//! headers a request carries (an event stream opened with nothing but
//! `Last-Event-ID`).
//!
//! Only what tests need: `http://` URLs, one request per connection
//! (`Connection: close`), `Content-Length` and chunked response bodies.

use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use belay_json::Value;

use crate::error::{Error, Result};

/// A response read to the end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawResponse {
    /// The status code.
    pub status: u16,
    /// Headers in order, names lowercased.
    pub headers: Vec<(String, String)>,
    /// The body, de-chunked.
    pub body: Vec<u8>,
}

impl RawResponse {
    /// The first value of header `name` (case-insensitive).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.as_str())
    }

    /// The body as text (lossy).
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The body as JSON.
    pub fn json(&self) -> Result<Value> {
        Ok(belay_json::parse_bytes(&self.body)?)
    }
}

/// Sends one request and reads the whole response, without following
/// redirects.
///
/// ```
/// use belay_testkit::{raw, TestServer};
///
/// let server = TestServer::start()?;
/// let response = raw::request("GET", &format!("{}/v1/nope", server.url()), &[], b"")?;
/// assert_eq!(response.status, 404);
/// assert_eq!(response.header("Content-Type"), Some("application/json"));
/// assert_eq!(response.json()?["error"]["code"], "not_found");
/// # Ok::<(), belay_testkit::Error>(())
/// ```
pub fn request(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<RawResponse> {
    let mut reader = send(method, url, headers, body)?;
    let (status, headers) = read_head(&mut reader)?;
    let chunked = header_value(&headers, "transfer-encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case("chunked"));
    let length = header_value(&headers, "content-length").and_then(|v| v.parse::<usize>().ok());
    let body = if chunked {
        let mut body = Vec::new();
        while let Some(chunk) = read_chunk(&mut reader)? {
            body.extend_from_slice(&chunk);
        }
        body
    } else if let Some(length) = length {
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body)?;
        body
    } else {
        let mut body = Vec::new();
        reader.read_to_end(&mut body)?;
        body
    };
    Ok(RawResponse {
        status,
        headers,
        body,
    })
}

/// One server-sent event as read off the wire.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawEvent {
    /// The `id:` field.
    pub id: Option<String>,
    /// The `event:` field.
    pub event: Option<String>,
    /// The `data:` lines, joined with newlines.
    pub data: String,
}

/// An event stream read by hand: `GET` with exactly the headers given.
pub struct RawEventStream {
    reader: BufReader<TcpStream>,
    /// Bytes of the stream not yet split into events.
    pending: String,
    /// The response status.
    pub status: u16,
}

impl RawEventStream {
    /// Opens `url` with `headers`; reads time out after `timeout`.
    pub fn open(url: &str, headers: &[(&str, &str)], timeout: Duration) -> Result<RawEventStream> {
        let mut all = vec![("Accept", "text/event-stream")];
        all.extend_from_slice(headers);
        let mut reader = send("GET", url, &all, b"")?;
        reader.get_ref().set_read_timeout(Some(timeout))?;
        let (status, headers) = read_head(&mut reader)?;
        let chunked = header_value(&headers, "transfer-encoding")
            .is_some_and(|v| v.eq_ignore_ascii_case("chunked"));
        if status == 200 && !chunked {
            return Err(Error::Invalid(
                "event streams are expected to be chunked".to_owned(),
            ));
        }
        Ok(RawEventStream {
            reader,
            pending: String::new(),
            status,
        })
    }

    /// The next event with a `data:` field, skipping comments, `retry:`
    /// frames and heartbeats. `None` when the stream ends.
    pub fn next_event(&mut self) -> Result<Option<RawEvent>> {
        loop {
            if let Some(end) = self.pending.find("\n\n") {
                let frame: String = self.pending.drain(..end + 2).collect();
                if let Some(event) = parse_frame(&frame) {
                    return Ok(Some(event));
                }
                continue;
            }
            match read_chunk(&mut self.reader)? {
                Some(chunk) => self.pending.push_str(&String::from_utf8_lossy(&chunk)),
                None => return Ok(None),
            }
        }
    }
}

impl fmt::Debug for RawEventStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawEventStream")
            .field("status", &self.status)
            .field("pending", &self.pending.len())
            .finish()
    }
}

fn parse_frame(frame: &str) -> Option<RawEvent> {
    let mut event = RawEvent::default();
    let mut data: Vec<&str> = Vec::new();
    for line in frame.lines() {
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "id" => event.id = Some(value.to_owned()),
            "event" => event.event = Some(value.to_owned()),
            "data" => data.push(value),
            _ => {}
        }
    }
    if data.is_empty() {
        return None;
    }
    event.data = data.join("\n");
    Some(event)
}

/// Splits `http://host:port/path?query` into the authority and the target.
fn split_url(url: &str) -> Result<(&str, &str)> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| Error::Invalid(format!("only http:// URLs are supported, got {url:?}")))?;
    Ok(match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    })
}

fn send(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<BufReader<TcpStream>> {
    let (authority, target) = split_url(url)?;
    let mut stream = TcpStream::connect(authority)?;
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    let mut head = format!(
        "{method} {target} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if !body.is_empty()
        && !headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("content-type"))
    {
        head.push_str("Content-Type: application/json\r\n");
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    Ok(BufReader::new(stream))
}

type Head = (u16, Vec<(String, String)>);

fn read_head(reader: &mut impl BufRead) -> Result<Head> {
    let status_line = read_line(reader)?;
    let status = status_line
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| Error::Invalid(format!("malformed status line {status_line:?}")))?;
    let mut headers = Vec::new();
    loop {
        let line = read_line(reader)?;
        if line.is_empty() {
            return Ok((status, headers));
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

/// One chunk of a chunked body; `None` after the last one.
fn read_chunk(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let size_line = read_line(reader)?;
    let size_text = size_line.split(';').next().unwrap_or("").trim();
    let size = usize::from_str_radix(size_text, 16)
        .map_err(|_| Error::Invalid(format!("malformed chunk size {size_line:?}")))?;
    if size == 0 {
        // Trailers, if any, end with an empty line.
        while !read_line(reader)?.is_empty() {}
        return Ok(None);
    }
    let mut chunk = vec![0u8; size];
    reader.read_exact(&mut chunk)?;
    read_line(reader)?;
    Ok(Some(chunk))
}

fn read_line(reader: &mut impl BufRead) -> Result<String> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "the connection closed mid-response",
        )));
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn urls_split_into_authority_and_target() {
        assert_eq!(
            split_url("http://127.0.0.1:7700/v1/jobs?limit=5").unwrap(),
            ("127.0.0.1:7700", "/v1/jobs?limit=5")
        );
        assert_eq!(split_url("http://h:1").unwrap(), ("h:1", "/"));
        assert!(split_url("https://h:1/").is_err());
    }

    #[test]
    fn chunked_bodies_are_reassembled() {
        let wire = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n1;ext=1\r\n!\r\n0\r\n\r\n";
        let mut reader = Cursor::new(&wire[..]);
        let (status, headers) = read_head(&mut reader).unwrap();
        assert_eq!(status, 200);
        assert_eq!(header_value(&headers, "transfer-encoding"), Some("chunked"));
        let mut body = Vec::new();
        while let Some(chunk) = read_chunk(&mut reader).unwrap() {
            body.extend(chunk);
        }
        assert_eq!(body, b"hello!");
    }

    #[test]
    fn frames_without_data_are_not_events() {
        assert_eq!(parse_frame("retry: 50\n\n"), None);
        assert_eq!(parse_frame(":\n\n"), None);
        let event = parse_frame("id: 7\nevent: job.enqueued\ndata: {\"a\":\ndata: 1}\n\n").unwrap();
        assert_eq!(event.id.as_deref(), Some("7"));
        assert_eq!(event.event.as_deref(), Some("job.enqueued"));
        assert_eq!(event.data, "{\"a\":\n1}");
    }

    #[test]
    fn header_lookups_ignore_case() {
        let response = RawResponse {
            status: 307,
            headers: vec![("location".into(), "http://b/v1/jobs".into())],
            body: Vec::new(),
        };
        assert_eq!(response.header("Location"), Some("http://b/v1/jobs"));
        assert_eq!(response.header("etag"), None);
    }
}
