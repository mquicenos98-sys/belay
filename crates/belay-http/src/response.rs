//! Responses and response bodies, including streamed ones.
//!
//! A [`Body::Stream`] is written with chunked encoding (or, for HTTP/1.0
//! clients, raw until the connection closes). The stream runs on the
//! connection's thread after the head has been sent, so a long-lived stream
//! occupies one connection slot for its lifetime; see
//! [`ServerConfig::max_connections`](crate::ServerConfig::max_connections).

use std::fmt;
use std::io::{self, Write};
use std::net::TcpStream;

use crate::headers::names::{CONTENT_TYPE, LOCATION};
use crate::headers::Headers;
use crate::proto::chunked::{encode_chunk, LAST_CHUNK};
use crate::status::StatusCode;
use crate::sync::ShutdownSignal;

/// Bytes buffered by [`BodyWriter`]'s `io::Write` impl before a chunk is
/// emitted without an explicit flush.
const FLUSH_THRESHOLD: usize = 16 * 1024;

/// A response body produced incrementally.
///
/// Implemented for closures, which is usually all that is needed:
///
/// ```
/// use std::io::Write;
/// use belay_http::{BodyWriter, Response};
///
/// let resp = Response::stream("text/plain", |out: &mut BodyWriter<'_>| {
///     for i in 0..3 {
///         writeln!(out, "line {i}")?;
///         out.flush()?; // one chunk per line
///     }
///     Ok(())
/// });
/// assert_eq!(resp.body.into_bytes()?, b"line 0\nline 1\nline 2\n");
/// # Ok::<(), std::io::Error>(())
/// ```
pub trait StreamBody: Send {
    /// Writes the body. Returning an error aborts the response; the
    /// connection is then closed because the message cannot be terminated
    /// cleanly.
    fn write_body(&mut self, out: &mut BodyWriter<'_>) -> io::Result<()>;
}

impl<F> StreamBody for F
where
    F: FnMut(&mut BodyWriter<'_>) -> io::Result<()> + Send,
{
    fn write_body(&mut self, out: &mut BodyWriter<'_>) -> io::Result<()> {
        self(out)
    }
}

/// A response body.
#[derive(Default)]
pub enum Body {
    /// No content.
    #[default]
    Empty,
    /// A complete body in memory, sent with `Content-Length`.
    Bytes(Vec<u8>),
    /// A body written incrementally, sent chunked.
    Stream(Box<dyn StreamBody>),
}

impl Body {
    /// The length when it is known up front.
    pub fn known_len(&self) -> Option<u64> {
        match self {
            Body::Empty => Some(0),
            Body::Bytes(bytes) => Some(bytes.len() as u64),
            Body::Stream(_) => None,
        }
    }

    /// The bytes of a non-streaming body.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Body::Empty => Some(&[]),
            Body::Bytes(bytes) => Some(bytes),
            Body::Stream(_) => None,
        }
    }

    /// Whether this is a streaming body.
    pub fn is_stream(&self) -> bool {
        matches!(self, Body::Stream(_))
    }

    /// Collects the body into memory, running a stream to completion. Meant
    /// for tests and for bodies known to be finite.
    pub fn into_bytes(self) -> io::Result<Vec<u8>> {
        match self {
            Body::Empty => Ok(Vec::new()),
            Body::Bytes(bytes) => Ok(bytes),
            Body::Stream(mut stream) => {
                let mut out = Vec::new();
                let mut writer = BodyWriter::new(&mut out);
                stream.write_body(&mut writer)?;
                writer.flush()?;
                drop(writer);
                Ok(out)
            }
        }
    }
}

impl fmt::Debug for Body {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Body::Empty => f.write_str("Empty"),
            Body::Bytes(bytes) => write!(f, "Bytes({} bytes)", bytes.len()),
            Body::Stream(_) => f.write_str("Stream"),
        }
    }
}

impl From<Vec<u8>> for Body {
    fn from(bytes: Vec<u8>) -> Self {
        Body::Bytes(bytes)
    }
}

impl From<String> for Body {
    fn from(text: String) -> Self {
        Body::Bytes(text.into_bytes())
    }
}

impl From<&str> for Body {
    fn from(text: &str) -> Self {
        Body::Bytes(text.as_bytes().to_vec())
    }
}

impl From<&[u8]> for Body {
    fn from(bytes: &[u8]) -> Self {
        Body::Bytes(bytes.to_vec())
    }
}

/// The sink a [`StreamBody`] writes into.
///
/// `io::Write::write` buffers; [`flush`](Write::flush) (or [`send`](Self::send))
/// emits everything buffered as one chunk and pushes it to the socket, so
/// the client sees data at flush boundaries. Writes are bounded by the
/// server's write timeout: a client that stops reading makes them fail
/// instead of blocking forever.
pub struct BodyWriter<'a> {
    out: &'a mut dyn Write,
    peer: Option<&'a TcpStream>,
    shutdown: Option<ShutdownSignal>,
    chunked: bool,
    buf: Vec<u8>,
    frame: Vec<u8>,
    written: u64,
}

impl<'a> BodyWriter<'a> {
    /// A writer that passes bytes to `out` without framing, for running a
    /// [`StreamBody`] outside the server. It always reports the client as
    /// connected and never as shutting down.
    pub fn new(out: &'a mut dyn Write) -> Self {
        BodyWriter {
            out,
            peer: None,
            shutdown: None,
            chunked: false,
            buf: Vec::new(),
            frame: Vec::new(),
            written: 0,
        }
    }

    pub(crate) fn for_connection(
        out: &'a mut dyn Write,
        peer: &'a TcpStream,
        shutdown: ShutdownSignal,
        chunked: bool,
    ) -> Self {
        BodyWriter {
            peer: Some(peer),
            shutdown: Some(shutdown),
            chunked,
            ..BodyWriter::new(out)
        }
    }

    /// Writes `data` and flushes it to the client immediately.
    pub fn send(&mut self, data: &[u8]) -> io::Result<()> {
        self.write_all(data)?;
        self.flush()
    }

    /// Body bytes handed to the socket so far (excluding chunk framing).
    pub fn bytes_written(&self) -> u64 {
        self.written
    }

    /// Whether the server has begun a graceful shutdown. Long-lived streams
    /// should finish promptly once this turns true, or they hold up the
    /// drain until the grace period runs out.
    pub fn is_shutting_down(&self) -> bool {
        self.shutdown
            .as_ref()
            .is_some_and(ShutdownSignal::is_triggered)
    }

    /// The server's shutdown signal, for waiting on it while idle.
    pub fn shutdown_signal(&self) -> Option<&ShutdownSignal> {
        self.shutdown.as_ref()
    }

    /// Checks, without blocking, whether the client has closed its end.
    ///
    /// A clean close is visible immediately; a vanished peer (cable pulled,
    /// process killed without FIN) is only detected when a write fails,
    /// which is why event streams should send periodic heartbeats. A client
    /// that half-closes its sending side right after the request is
    /// indistinguishable from one that left and is reported as gone.
    pub fn is_client_connected(&self) -> bool {
        let Some(peer) = self.peer else {
            return true;
        };
        if peer.set_nonblocking(true).is_err() {
            return false;
        }
        let mut probe = [0u8; 1];
        let connected = match peer.peek(&mut probe) {
            Ok(0) => false,
            // Pipelined bytes from a client that is still there.
            Ok(_) => true,
            Err(e) => matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ),
        };
        peer.set_nonblocking(false).is_ok() && connected
    }

    fn emit(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        if self.chunked {
            self.frame.clear();
            encode_chunk(&mut self.frame, &self.buf);
            self.out.write_all(&self.frame)?;
        } else {
            self.out.write_all(&self.buf)?;
        }
        self.written += self.buf.len() as u64;
        self.buf.clear();
        Ok(())
    }

    /// Flushes buffered data and, for chunked bodies, writes the final
    /// chunk.
    pub(crate) fn finish(mut self) -> io::Result<()> {
        self.emit()?;
        if self.chunked {
            self.out.write_all(LAST_CHUNK)?;
        }
        self.out.flush()
    }
}

impl Write for BodyWriter<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= FLUSH_THRESHOLD {
            self.emit()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.emit()?;
        self.out.flush()
    }
}

impl fmt::Debug for BodyWriter<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BodyWriter")
            .field("chunked", &self.chunked)
            .field("buffered", &self.buf.len())
            .field("written", &self.written)
            .finish()
    }
}

/// An HTTP response.
///
/// ```
/// use belay_http::{Response, StatusCode};
///
/// let resp = Response::json_bytes(r#"{"id":"01H"}"#)
///     .with_status(StatusCode::CREATED)
///     .header("Location", "/v1/jobs/01H");
/// assert_eq!(resp.status, 201);
/// assert_eq!(resp.headers.get("content-type"), Some("application/json"));
/// ```
pub struct Response {
    /// The status code.
    pub status: StatusCode,
    /// Header fields. Framing fields are managed by the server and ignored
    /// here.
    pub headers: Headers,
    /// The body.
    pub body: Body,
}

impl Response {
    /// An empty response with `status`.
    pub fn new(status: StatusCode) -> Response {
        Response {
            status,
            headers: Headers::new(),
            body: Body::Empty,
        }
    }

    /// An empty response with `status`; alias of [`Response::new`].
    pub fn status(status: StatusCode) -> Response {
        Response::new(status)
    }

    /// `200 OK` with a UTF-8 text body.
    pub fn text(body: impl Into<String>) -> Response {
        Response::bytes("text/plain; charset=utf-8", body.into().into_bytes())
    }

    /// `200 OK` with an already-serialized JSON body.
    pub fn json_bytes(body: impl Into<Vec<u8>>) -> Response {
        Response::bytes("application/json", body)
    }

    /// `200 OK` with `body` of the given content type.
    pub fn bytes(content_type: &str, body: impl Into<Vec<u8>>) -> Response {
        Response {
            status: StatusCode::OK,
            headers: [(CONTENT_TYPE, content_type)].into_iter().collect(),
            body: Body::Bytes(body.into()),
        }
    }

    /// `200 OK` with a streamed body.
    pub fn stream(content_type: &str, body: impl StreamBody + 'static) -> Response {
        Response {
            status: StatusCode::OK,
            headers: [(CONTENT_TYPE, content_type)].into_iter().collect(),
            body: Body::Stream(Box::new(body)),
        }
    }

    /// `204 No Content`.
    pub fn no_content() -> Response {
        Response::new(StatusCode::NO_CONTENT)
    }

    /// A redirect to `location` with the given 3xx status.
    pub fn redirect(status: StatusCode, location: &str) -> Response {
        Response::new(status).header(LOCATION, location)
    }

    /// `status` with `message` as a plain-text body.
    pub fn error(status: StatusCode, message: &str) -> Response {
        let mut body = String::with_capacity(message.len() + 1);
        body.push_str(message);
        body.push('\n');
        Response::text(body).with_status(status)
    }

    /// Replaces the status (builder style).
    #[must_use]
    pub fn with_status(mut self, status: StatusCode) -> Self {
        self.status = status;
        self
    }

    /// Sets a header, replacing existing values (builder style).
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name, value);
        self
    }

    /// Adds a header, keeping existing values (builder style).
    #[must_use]
    pub fn append_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.append(name, value);
        self
    }

    /// Replaces the body (builder style).
    #[must_use]
    pub fn with_body(mut self, body: impl Into<Body>) -> Self {
        self.body = body.into();
        self
    }

    /// The body bytes, unless the body is a stream.
    pub fn body_bytes(&self) -> Option<&[u8]> {
        self.body.as_bytes()
    }
}

impl fmt::Debug for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("body", &self.body)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors_set_content_type_and_status() {
        let text = Response::text("hi");
        assert_eq!(text.status, StatusCode::OK);
        assert_eq!(
            text.headers.get("Content-Type"),
            Some("text/plain; charset=utf-8")
        );
        assert_eq!(text.body_bytes(), Some(&b"hi"[..]));

        let err = Response::error(StatusCode::CONFLICT, "schedule exists");
        assert_eq!(err.status, 409);
        assert_eq!(err.body_bytes(), Some(&b"schedule exists\n"[..]));

        let redirect = Response::redirect(StatusCode::TEMPORARY_REDIRECT, "http://node-2/v1/lease");
        assert_eq!(
            redirect.headers.get("location"),
            Some("http://node-2/v1/lease")
        );
        assert_eq!(Response::no_content().body.known_len(), Some(0));
    }

    #[test]
    fn header_replaces_and_append_accumulates() {
        let resp = Response::new(StatusCode::OK)
            .header("Vary", "Origin")
            .header("vary", "Accept")
            .append_header("Vary", "Accept-Encoding");
        let values: Vec<_> = resp.headers.get_all("vary").collect();
        assert_eq!(values, ["Accept", "Accept-Encoding"]);
    }

    #[test]
    fn chunked_writer_frames_each_flush() {
        let mut wire = Vec::new();
        {
            let mut out = BodyWriter::new(&mut wire);
            out.chunked = true;
            out.write_all(b"ab").expect("vec write");
            out.write_all(b"c").expect("vec write");
            out.flush().expect("vec flush");
            out.flush().expect("empty flush writes nothing");
            out.send(b"de").expect("vec write");
            assert_eq!(out.bytes_written(), 5);
            out.finish().expect("vec write");
        }
        assert_eq!(wire, b"3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n");
    }

    #[test]
    fn large_writes_are_emitted_without_flush() {
        let mut wire = Vec::new();
        let mut out = BodyWriter::new(&mut wire);
        out.write_all(&vec![7u8; FLUSH_THRESHOLD + 1])
            .expect("vec write");
        assert_eq!(out.bytes_written(), FLUSH_THRESHOLD as u64 + 1);
    }

    #[test]
    fn detached_writer_reports_connected_and_not_closing() {
        let mut sink = io::sink();
        let out = BodyWriter::new(&mut sink);
        assert!(out.is_client_connected());
        assert!(!out.is_shutting_down());
        assert!(out.shutdown_signal().is_none());
    }

    #[test]
    fn into_bytes_runs_streams() {
        let mut count = 0;
        let body = Body::Stream(Box::new(move |out: &mut BodyWriter<'_>| {
            while count < 3 {
                count += 1;
                out.send(format!("{count};").as_bytes())?;
            }
            Ok(())
        }));
        assert!(body.is_stream());
        assert_eq!(body.known_len(), None);
        assert_eq!(body.into_bytes().expect("finite stream"), b"1;2;3;");
    }

    #[test]
    fn stream_errors_propagate_from_into_bytes() {
        let body = Body::Stream(Box::new(|_: &mut BodyWriter<'_>| {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "client left"))
        }));
        let err = body.into_bytes().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }
}
