//! One connection's lifetime: read a request, run the handler, write the
//! response, repeat until the connection closes, times out, or the server
//! shuts down.
//!
//! Sockets are read with a short timeout (`poll_interval`) so the loop can
//! notice a shutdown while it waits for the next request on an idle
//! keep-alive connection; the real deadlines (head, body, keep-alive) are
//! tracked separately with `Instant`s. A request whose head has started
//! arriving counts as in flight and is finished even during shutdown; only
//! connections with nothing buffered are dropped early.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::buf::InBuf;
use crate::date::now_http_date;
use crate::handler::Handler;
use crate::headers::names::{CONNECTION, CONTENT_TYPE, EXPECT};
use crate::headers::Headers;
use crate::method::{Method, Version};
use crate::middleware::panic_message;
use crate::proto::chunked::ChunkedDecoder;
use crate::proto::framing::{
    check_host, content_length, request_framing, wants_keep_alive, BodyFraming,
};
use crate::proto::parse::{ParseError, RequestParser};
use crate::proto::write::{encode_response_head, OutFraming, ResponseMeta};
use crate::request::Request;
use crate::response::{Body, BodyWriter, Response};
use crate::server::config::{ServerConfig, ServerError};
use crate::status::StatusCode;
use crate::sync::ShutdownSignal;

const READ_CHUNK: usize = 16 * 1024;
/// Declared body lengths are trusted for preallocation only up to this.
const INITIAL_BODY_CAPACITY: usize = 64 * 1024;
/// Bodies up to this size go out in the same write as the head.
const INLINE_BODY_LIMIT: usize = 64 * 1024;
/// After an error response, unread request bytes are drained for at most
/// this long before closing, so the close does not turn into a reset that
/// destroys the response before the client reads it.
const LINGER_TIME: Duration = Duration::from_millis(500);
const LINGER_BYTES: usize = 256 * 1024;

/// Counters shared by the accept loop and all connections.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    pub(crate) accepted: AtomicU64,
    pub(crate) rejected: AtomicU64,
    pub(crate) requests: AtomicU64,
    pub(crate) protocol_errors: AtomicU64,
    pub(crate) active: AtomicUsize,
}

/// State shared by every connection of one server.
pub(crate) struct Shared {
    pub(crate) config: ServerConfig,
    pub(crate) handler: Arc<dyn Handler>,
    pub(crate) shutdown: ShutdownSignal,
    pub(crate) counters: Counters,
}

impl Shared {
    /// More than 7/8 of the connection slots are taken: stop offering
    /// keep-alive so idle connections release their slots.
    fn under_pressure(&self) -> bool {
        let max = self.config.max_connections;
        self.counters.active.load(Ordering::Relaxed) > max - max / 8
    }
}

/// Serves one accepted connection until it closes.
pub(crate) fn serve(stream: TcpStream, peer: SocketAddr, shared: &Shared) {
    shared.counters.active.fetch_add(1, Ordering::Relaxed);
    if let Ok(conn) = Connection::new(stream, peer, shared) {
        conn.run();
    }
    shared.counters.active.fetch_sub(1, Ordering::Relaxed);
}

enum Outcome {
    /// Wait for the next request.
    KeepAlive,
    /// Response complete; close gracefully.
    Close,
    /// An error response was sent while the client may still be sending;
    /// drain briefly before closing.
    Linger,
    /// The connection is unusable (I/O error, peer gone, idle timeout).
    Abort,
}

enum Fill {
    Data,
    Eof,
    Timeout,
    Shutdown,
}

enum BodyError {
    Parse(ParseError),
    Timeout,
    Closed,
}

struct Connection<'s> {
    stream: TcpStream,
    peer: SocketAddr,
    shared: &'s Shared,
    buf: InBuf,
    served: usize,
}

fn deadline_after(timeout: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(timeout)
        .unwrap_or_else(|| now + Duration::from_secs(365 * 86_400))
}

fn is_timeout(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

impl<'s> Connection<'s> {
    fn new(stream: TcpStream, peer: SocketAddr, shared: &'s Shared) -> io::Result<Self> {
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(shared.config.poll_interval))?;
        stream.set_write_timeout(Some(shared.config.write_timeout))?;
        Ok(Connection {
            stream,
            peer,
            shared,
            buf: InBuf::new(),
            served: 0,
        })
    }

    fn run(mut self) {
        loop {
            match self.serve_one() {
                Outcome::KeepAlive => {}
                Outcome::Close => {
                    let _ = self.stream.shutdown(Shutdown::Write);
                    return;
                }
                Outcome::Linger => {
                    self.linger();
                    return;
                }
                Outcome::Abort => return,
            }
        }
    }

    fn serve_one(&mut self) -> Outcome {
        let shared = self.shared;
        let config = &shared.config;
        let first = self.served == 0;
        let idle_deadline = deadline_after(if first {
            config.header_timeout
        } else {
            config.keep_alive_timeout
        });
        let mut head_deadline = None;
        let mut parser = RequestParser::new(config.head_limits());
        let head = loop {
            if !self.buf.is_empty() {
                if head_deadline.is_none() {
                    head_deadline = Some(if first {
                        idle_deadline
                    } else {
                        deadline_after(config.header_timeout)
                    });
                }
                match parser.parse(self.buf.as_slice()) {
                    Ok(Some((head, len))) => {
                        self.buf.consume(len);
                        break head;
                    }
                    Ok(None) => {}
                    Err(err) => return self.reject(err.status(), &err.to_string()),
                }
            }
            let idle = self.buf.is_empty();
            match self.fill(head_deadline.unwrap_or(idle_deadline), idle) {
                Ok(Fill::Data) => {}
                Ok(Fill::Timeout) if !idle => {
                    return self.reject(
                        StatusCode::REQUEST_TIMEOUT,
                        "timed out waiting for the request head",
                    )
                }
                // Idle timeout, shutdown while idle, EOF or a socket error:
                // nothing is owed to the client.
                Ok(_) | Err(_) => return Outcome::Abort,
            }
        };
        self.shared
            .counters
            .requests
            .fetch_add(1, Ordering::Relaxed);

        if head.method == Method::Connect {
            return self.reject(StatusCode::NOT_IMPLEMENTED, "CONNECT is not supported");
        }
        if let Err(err) = check_host(&head) {
            return self.reject(err.status(), &err.to_string());
        }
        if head.target == "*" && head.method != Method::Options {
            return self.reject(
                StatusCode::BAD_REQUEST,
                "asterisk-form target is only valid for OPTIONS",
            );
        }
        let framing = match request_framing(&head) {
            Ok(framing) => framing,
            Err(err) => return self.reject(err.status(), &err.to_string()),
        };
        let limit = config.max_body_bytes as u64;
        if let BodyFraming::Length(len) = framing {
            if len > limit {
                return self.reject(
                    StatusCode::CONTENT_TOO_LARGE,
                    &format!("request body of {len} bytes exceeds the {limit} byte limit"),
                );
            }
        }
        match head.headers.get(EXPECT) {
            None => {}
            Some(expect) if expect.eq_ignore_ascii_case("100-continue") => {
                // Only worth sending if the client is actually waiting for
                // it: HTTP/1.1, a body is coming, and none has arrived yet.
                let waiting = head.version == Version::Http11
                    && framing != BodyFraming::Empty
                    && self.buf.is_empty();
                if waiting && self.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").is_err() {
                    return Outcome::Abort;
                }
            }
            Some(_) => {
                return self.reject(
                    StatusCode::EXPECTATION_FAILED,
                    "only the 100-continue expectation is supported",
                )
            }
        }
        let body = match self.read_body(framing) {
            Ok(body) => body,
            Err(BodyError::Parse(err)) => return self.reject(err.status(), &err.to_string()),
            Err(BodyError::Timeout) => {
                return self.reject(
                    StatusCode::REQUEST_TIMEOUT,
                    "timed out reading the request body",
                )
            }
            Err(BodyError::Closed) => return Outcome::Abort,
        };

        let client_keep_alive = wants_keep_alive(head.version, &head.headers);
        let mut req = Request::from_head(head, body, self.peer);
        req.extensions.insert(self.shared.shutdown.clone());
        let response = self.call_handler(&mut req);
        self.served += 1;
        let keep_alive = client_keep_alive && self.may_keep_alive();
        self.write_response(response, &req, keep_alive)
    }

    fn may_keep_alive(&self) -> bool {
        let config = &self.shared.config;
        !config.keep_alive_timeout.is_zero()
            && !self.shared.shutdown.is_triggered()
            && (config.max_requests_per_connection == 0
                || self.served < config.max_requests_per_connection)
            && !self.shared.under_pressure()
    }

    /// Reads once into the buffer, waiting until `deadline`. While `idle`
    /// (no partial request buffered) a shutdown ends the wait early.
    fn fill(&mut self, deadline: Instant, idle: bool) -> io::Result<Fill> {
        if idle && self.shared.shutdown.is_triggered() {
            return Ok(Fill::Shutdown);
        }
        loop {
            match self.buf.read_from(&mut &self.stream, READ_CHUNK) {
                Ok(0) => return Ok(Fill::Eof),
                Ok(_) => return Ok(Fill::Data),
                Err(e) if is_timeout(&e) => {
                    if Instant::now() >= deadline {
                        return Ok(Fill::Timeout);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    fn fill_body(&mut self) -> Result<(), BodyError> {
        let deadline = deadline_after(self.shared.config.body_timeout);
        match self.fill(deadline, false) {
            Ok(Fill::Data) => Ok(()),
            Ok(Fill::Timeout) => Err(BodyError::Timeout),
            Ok(Fill::Eof | Fill::Shutdown) | Err(_) => Err(BodyError::Closed),
        }
    }

    fn read_body(&mut self, framing: BodyFraming) -> Result<Vec<u8>, BodyError> {
        match framing {
            BodyFraming::Empty | BodyFraming::UntilClose => Ok(Vec::new()),
            BodyFraming::Length(len) => {
                // The caller checked `len` against max_body_bytes (a usize).
                let len = len as usize;
                let mut body = Vec::with_capacity(len.min(INITIAL_BODY_CAPACITY));
                while body.len() < len {
                    if self.buf.is_empty() {
                        self.fill_body()?;
                    }
                    let missing = len - body.len();
                    self.buf.drain_into(&mut body, missing);
                }
                Ok(body)
            }
            BodyFraming::Chunked => {
                let mut decoder = ChunkedDecoder::new(self.shared.config.max_body_bytes as u64);
                let mut body = Vec::new();
                loop {
                    let used = decoder
                        .decode(self.buf.as_slice(), &mut body)
                        .map_err(BodyError::Parse)?;
                    self.buf.consume(used);
                    if decoder.is_done() {
                        return Ok(body);
                    }
                    self.fill_body()?;
                }
            }
        }
    }

    fn call_handler(&self, req: &mut Request) -> Response {
        // A panicking handler may leave its own state inconsistent; that is
        // visible to it through poisoned locks. The connection itself is
        // untouched: the request was fully read before the call.
        match catch_unwind(AssertUnwindSafe(|| self.shared.handler.call(req))) {
            Ok(response) => response,
            Err(payload) => {
                self.shared.config.report(ServerError::HandlerPanic {
                    method: req.method.clone(),
                    path: req.path.clone(),
                    message: panic_message(payload.as_ref()),
                });
                internal_error()
            }
        }
    }

    fn write_response(
        &mut self,
        mut response: Response,
        req: &Request,
        mut keep_alive: bool,
    ) -> Outcome {
        let shared = self.shared;
        let config = &shared.config;
        if response.status.is_informational() {
            config.report(ServerError::InvalidResponse {
                method: req.method.clone(),
                path: req.path.clone(),
                reason: format!(
                    "interim status {} used as a final response",
                    response.status
                ),
            });
            response = internal_error();
        }
        if response.headers.has_token(CONNECTION, "close") || self.shared.shutdown.is_triggered() {
            keep_alive = false;
        }
        let is_head = req.method == Method::Head;
        let status = response.status;
        // A HEAD or 304 response may announce the length the full response
        // would have had (RFC 9110 §8.6).
        let declared = content_length(&response.headers).ok().flatten();
        let framing = match &response.body {
            _ if status == StatusCode::NO_CONTENT => OutFraming::None,
            _ if status == StatusCode::NOT_MODIFIED => {
                declared.map_or(OutFraming::None, OutFraming::Length)
            }
            Body::Empty if is_head => OutFraming::Length(declared.unwrap_or(0)),
            Body::Empty => OutFraming::Length(0),
            Body::Bytes(bytes) => OutFraming::Length(bytes.len() as u64),
            Body::Stream(_) if req.version == Version::Http11 => OutFraming::Chunked,
            Body::Stream(_) => {
                keep_alive = false;
                OutFraming::Close
            }
        };
        let connection = match (keep_alive, req.version) {
            (false, _) => Some("close"),
            (true, Version::Http10) => Some("keep-alive"),
            (true, Version::Http11) => None,
        };
        let date = now_http_date();
        let meta = ResponseMeta {
            framing,
            connection,
            date: Some(&date),
            server: config.server_header.as_deref(),
        };
        let mut out = Vec::with_capacity(512);
        if let Err(err) = encode_response_head(&mut out, status, &response.headers, &meta) {
            config.report(ServerError::InvalidResponse {
                method: req.method.clone(),
                path: req.path.clone(),
                reason: err.to_string(),
            });
            return match self
                .write_error(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
            {
                Ok(()) => Outcome::Close,
                Err(_) => Outcome::Abort,
            };
        }
        let send_body = !is_head && !status.forbids_body();
        let result = match response.body {
            Body::Bytes(bytes) if send_body => {
                if bytes.len() <= INLINE_BODY_LIMIT {
                    out.extend_from_slice(&bytes);
                    self.write_all(&out)
                } else {
                    self.write_all(&out).and_then(|()| self.write_all(&bytes))
                }
            }
            Body::Stream(stream) if send_body => self
                .write_all(&out)
                .and_then(|()| self.write_stream(stream, framing, req)),
            _ => self.write_all(&out),
        };
        match result {
            Ok(()) if keep_alive => Outcome::KeepAlive,
            Ok(()) => Outcome::Close,
            Err(_) => Outcome::Abort,
        }
    }

    fn write_stream(
        &self,
        mut stream: Box<dyn crate::response::StreamBody>,
        framing: OutFraming,
        req: &Request,
    ) -> io::Result<()> {
        let mut sink: &TcpStream = &self.stream;
        let mut writer = BodyWriter::for_connection(
            &mut sink,
            &self.stream,
            self.shared.shutdown.clone(),
            framing == OutFraming::Chunked,
        );
        match catch_unwind(AssertUnwindSafe(|| stream.write_body(&mut writer))) {
            Ok(Ok(())) => writer.finish(),
            Ok(Err(err)) => Err(err),
            Err(payload) => {
                self.shared.config.report(ServerError::HandlerPanic {
                    method: req.method.clone(),
                    path: req.path.clone(),
                    message: panic_message(payload.as_ref()),
                });
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    "streaming body panicked",
                ))
            }
        }
    }

    fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        (&self.stream).write_all(bytes)
    }

    /// Answers a request that cannot be served and schedules the close.
    fn reject(&mut self, status: StatusCode, message: &str) -> Outcome {
        self.shared
            .counters
            .protocol_errors
            .fetch_add(1, Ordering::Relaxed);
        match self.write_error(status, message) {
            Ok(()) => Outcome::Linger,
            Err(_) => Outcome::Abort,
        }
    }

    fn write_error(&self, status: StatusCode, message: &str) -> io::Result<()> {
        let body = format!("{message}\n");
        let date = now_http_date();
        let headers: Headers = [(CONTENT_TYPE, "text/plain; charset=utf-8")]
            .into_iter()
            .collect();
        let meta = ResponseMeta {
            framing: OutFraming::Length(body.len() as u64),
            connection: Some("close"),
            date: Some(&date),
            server: self.shared.config.server_header.as_deref(),
        };
        let mut out = Vec::with_capacity(256 + body.len());
        encode_response_head(&mut out, status, &headers, &meta)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        out.extend_from_slice(body.as_bytes());
        self.write_all(&out)
    }

    /// Half-closes, then discards what the client is still sending (for a
    /// bounded time and volume) so our close does not reset the connection
    /// under the error response.
    fn linger(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Write);
        let deadline = Instant::now() + LINGER_TIME;
        let mut scratch = [0u8; 4096];
        let mut drained = 0;
        while drained < LINGER_BYTES && Instant::now() < deadline {
            match (&self.stream).read(&mut scratch) {
                Ok(0) => break,
                Ok(n) => drained += n,
                Err(e) if is_timeout(&e) || e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    }
}

fn internal_error() -> Response {
    Response::error(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
}

/// Refuses a connection the pool has no room for. Runs on the accept
/// thread, so it must not block: the response fits in any socket buffer,
/// and the request bytes that already arrived are drained without waiting
/// so closing does not reset the connection before the 503 is read.
pub(crate) fn refuse(stream: TcpStream) {
    const BODY: &str = "server is at connection capacity\n";
    let response = format!(
        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nRetry-After: 1\r\nConnection: close\r\n\r\n{BODY}",
        BODY.len()
    );
    let _ = stream.set_write_timeout(Some(Duration::from_millis(250)));
    let _ = (&stream).write_all(response.as_bytes());
    let _ = stream.shutdown(Shutdown::Write);
    if stream.set_nonblocking(true).is_ok() {
        let mut scratch = [0u8; 4096];
        for _ in 0..16 {
            match (&stream).read(&mut scratch) {
                Ok(n) if n > 0 => {}
                _ => break,
            }
        }
    }
}
