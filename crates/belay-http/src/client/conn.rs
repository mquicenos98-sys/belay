//! Client connections: the idle pool, one request/response exchange, and
//! streaming response bodies that return their connection to the pool once
//! fully read.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::buf::InBuf;
use crate::error::{Error, Result};
use crate::headers::names::CONNECTION;
use crate::headers::Headers;
use crate::method::Method;
use crate::proto::chunked::ChunkedBody;
use crate::proto::framing::BodyFraming;
use crate::proto::parse::{HeadLimits, ParseError, ResponseHead, ResponseParser};
use crate::proto::write::{encode_request_head, push_field};
use crate::status::StatusCode;
use crate::sync::lock;
use crate::url::Url;

const READ_CHUNK: usize = 16 * 1024;
/// Bodies up to this size go out in the same write as the head.
const INLINE_BODY_LIMIT: usize = 64 * 1024;

/// Connections are pooled per `(host, port)`.
pub(crate) type PoolKey = (String, u16);

struct IdleConn {
    stream: TcpStream,
    since: Instant,
}

/// Idle keep-alive connections, most recently used last.
pub(crate) struct Pool {
    idle: Mutex<HashMap<PoolKey, Vec<IdleConn>>>,
    max_per_host: usize,
    idle_timeout: Duration,
}

impl Pool {
    pub(crate) fn new(max_per_host: usize, idle_timeout: Duration) -> Self {
        Pool {
            idle: Mutex::new(HashMap::new()),
            max_per_host,
            idle_timeout,
        }
    }

    /// The most recently used live connection to `key`, if any. Expired
    /// and dead connections found on the way are dropped.
    pub(crate) fn checkout(&self, key: &PoolKey) -> Option<TcpStream> {
        loop {
            let candidate = {
                let mut idle = lock(&self.idle);
                let list = idle.get_mut(key)?;
                let candidate = list.pop();
                if list.is_empty() {
                    idle.remove(key);
                }
                candidate?
            };
            // The liveness probe is a syscall; keep it outside the lock.
            if candidate.since.elapsed() < self.idle_timeout && is_alive(&candidate.stream) {
                return Some(candidate.stream);
            }
        }
    }

    /// Returns a connection whose last response was fully read.
    pub(crate) fn checkin(&self, key: PoolKey, stream: TcpStream) {
        if self.max_per_host == 0 {
            return;
        }
        let mut idle = lock(&self.idle);
        let list = idle.entry(key).or_default();
        list.retain(|c| c.since.elapsed() < self.idle_timeout);
        if list.len() >= self.max_per_host {
            list.remove(0);
        }
        list.push(IdleConn {
            stream,
            since: Instant::now(),
        });
    }

    pub(crate) fn idle_count(&self) -> usize {
        lock(&self.idle).values().map(Vec::len).sum()
    }

    pub(crate) fn clear(&self) {
        lock(&self.idle).clear();
    }
}

impl fmt::Debug for Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool")
            .field("idle", &self.idle_count())
            .field("max_per_host", &self.max_per_host)
            .finish()
    }
}

/// Whether an idle pooled connection is still usable: the peer has not
/// closed it and has not sent anything unsolicited (which would desync the
/// next response).
fn is_alive(stream: &TcpStream) -> bool {
    if stream.set_nonblocking(true).is_err() {
        return false;
    }
    let mut probe = [0u8; 1];
    let alive = matches!(
        stream.peek(&mut probe),
        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock
    );
    stream.set_nonblocking(false).is_ok() && alive
}

/// Opens a TCP connection, trying each resolved address in turn.
pub(crate) fn connect(host: &str, port: u16, timeout: Duration) -> Result<TcpStream> {
    let addrs = (host, port).to_socket_addrs()?;
    let mut last_error = None;
    for addr in addrs {
        let attempt = if timeout.is_zero() {
            TcpStream::connect(addr)
        } else {
            TcpStream::connect_timeout(&addr, timeout)
        };
        match attempt {
            Ok(stream) => {
                stream.set_nodelay(true)?;
                return Ok(stream);
            }
            Err(e) => last_error = Some(e),
        }
    }
    Err(match last_error {
        Some(e) => Error::from_io(e, "connect"),
        None => Error::Io(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{host} did not resolve to any address"),
        )),
    })
}

/// A connection in use for one exchange.
pub(crate) struct Conn {
    pub(crate) stream: TcpStream,
    pub(crate) buf: InBuf,
    pub(crate) key: PoolKey,
    /// Taken from the pool rather than freshly connected.
    pub(crate) reused: bool,
}

impl Conn {
    pub(crate) fn new(stream: TcpStream, key: PoolKey, reused: bool) -> Self {
        Conn {
            stream,
            buf: InBuf::new(),
            key,
            reused,
        }
    }
}

/// One outgoing request.
pub(crate) struct Outgoing<'a> {
    pub(crate) method: &'a Method,
    pub(crate) url: &'a Url,
    pub(crate) headers: &'a Headers,
    pub(crate) body: &'a [u8],
    pub(crate) user_agent: &'a str,
    /// Ask the server to close the connection afterwards.
    pub(crate) close: bool,
    pub(crate) read_timeout: Option<Duration>,
    pub(crate) write_timeout: Option<Duration>,
    pub(crate) max_head_bytes: usize,
}

/// A failed exchange. `stale` means the failure happened on a reused
/// connection before any response byte arrived, the signature of a
/// connection the server closed while it sat in the pool; such requests
/// can be retried on a fresh connection if they are idempotent.
pub(crate) struct Failure {
    pub(crate) error: Error,
    pub(crate) stale: bool,
}

impl Failure {
    fn fatal(error: impl Into<Error>) -> Self {
        Failure {
            error: error.into(),
            stale: false,
        }
    }
}

fn non_zero(timeout: Option<Duration>) -> Option<Duration> {
    timeout.filter(|t| !t.is_zero())
}

/// Sends the request and reads the final response head, skipping interim
/// 1xx responses.
pub(crate) fn exchange(conn: &mut Conn, out: &Outgoing<'_>) -> Result<ResponseHead, Failure> {
    conn.stream
        .set_read_timeout(non_zero(out.read_timeout))
        .map_err(Failure::fatal)?;
    conn.stream
        .set_write_timeout(non_zero(out.write_timeout))
        .map_err(Failure::fatal)?;

    let mut wire = Vec::with_capacity(256 + out.body.len().min(INLINE_BODY_LIMIT));
    encode_request_head(
        &mut wire,
        out.method,
        &out.url.request_target(),
        &out.url.authority(),
        out.user_agent,
        out.headers,
        out.body.len(),
    )
    .map_err(Failure::fatal)?;
    if out.close && !out.headers.contains(CONNECTION) {
        // Splice `Connection: close` in before the blank line.
        wire.truncate(wire.len() - 2);
        push_field(&mut wire, CONNECTION, "close").map_err(Failure::fatal)?;
        wire.extend_from_slice(b"\r\n");
    }
    let inline = out.body.len() <= INLINE_BODY_LIMIT;
    if inline {
        wire.extend_from_slice(out.body);
    }
    let mut written = (&conn.stream).write_all(&wire);
    if written.is_ok() && !inline {
        written = (&conn.stream).write_all(out.body);
    }
    if let Err(e) = written {
        let timed_out = matches!(
            e.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        );
        return Err(Failure {
            stale: conn.reused && !timed_out,
            error: Error::from_io(e, "write"),
        });
    }

    let limits = HeadLimits {
        start_line: out.max_head_bytes,
        head: out.max_head_bytes,
        headers: 256,
    };
    let mut received_any = false;
    loop {
        let mut parser = ResponseParser::new(limits);
        let head = loop {
            if !conn.buf.is_empty() {
                if let Some((head, len)) =
                    parser.parse(conn.buf.as_slice()).map_err(Failure::fatal)?
                {
                    conn.buf.consume(len);
                    break head;
                }
            }
            match conn.buf.read_from(&mut &conn.stream, READ_CHUNK) {
                Ok(0) => {
                    return Err(Failure {
                        error: Error::ConnectionClosed,
                        stale: conn.reused && !received_any,
                    })
                }
                Ok(_) => received_any = true,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    let timed_out = matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    );
                    return Err(Failure {
                        stale: conn.reused && !received_any && !timed_out,
                        error: Error::from_io(e, "read"),
                    });
                }
            }
        };
        if head.status.is_informational() && head.status != StatusCode::SWITCHING_PROTOCOLS {
            continue;
        }
        return Ok(head);
    }
}

enum Framing {
    Length(u64),
    Chunked(ChunkedBody),
    UntilClose,
    Done,
}

/// A response body being read from the connection.
///
/// Implements [`Read`]. When the body has been read to the end and the
/// connection may be reused, it goes back to the client's pool; dropping a
/// partially read body closes the connection instead.
pub struct ResponseBody {
    conn: Option<Conn>,
    framing: Framing,
    keep_alive: bool,
    pool: Arc<Pool>,
    limit: u64,
}

impl ResponseBody {
    pub(crate) fn new(
        conn: Conn,
        framing: BodyFraming,
        keep_alive: bool,
        pool: Arc<Pool>,
        limit: u64,
    ) -> Self {
        let framing = match framing {
            BodyFraming::Empty => Framing::Done,
            BodyFraming::Length(n) => Framing::Length(n),
            BodyFraming::Chunked => Framing::Chunked(ChunkedBody::new(limit)),
            BodyFraming::UntilClose => Framing::UntilClose,
        };
        let mut body = ResponseBody {
            conn: Some(conn),
            framing,
            keep_alive,
            pool,
            limit,
        };
        if matches!(body.framing, Framing::Done) {
            body.finish();
        }
        body
    }

    /// The declared length, when the response has a `Content-Length`.
    pub fn content_length(&self) -> Option<u64> {
        match self.framing {
            Framing::Length(n) => Some(n),
            _ => None,
        }
    }

    /// Whether the whole body has been read.
    pub fn is_complete(&self) -> bool {
        matches!(self.framing, Framing::Done)
    }

    /// Reads the rest of the body into memory, up to the client's
    /// response-size limit.
    pub fn read_to_vec(&mut self) -> Result<Vec<u8>> {
        if let Framing::Length(n) = self.framing {
            if n > self.limit {
                return Err(Error::BodyTooLarge { limit: self.limit });
            }
        }
        let initial = self.content_length().unwrap_or(0).min(1 << 20) as usize;
        let mut body = Vec::with_capacity(initial);
        let mut chunk = [0u8; READ_CHUNK];
        loop {
            let n = self.read(&mut chunk).map_err(body_error)?;
            if n == 0 {
                return Ok(body);
            }
            if (body.len() + n) as u64 > self.limit {
                return Err(Error::BodyTooLarge { limit: self.limit });
            }
            body.extend_from_slice(&chunk[..n]);
        }
    }

    /// Reads and discards up to `max` bytes so a small unwanted body (a
    /// redirect's, say) does not cost the connection.
    pub(crate) fn discard(mut self, max: usize) {
        let mut chunk = [0u8; 4096];
        let mut total = 0;
        while total < max {
            match self.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => total += n,
            }
        }
    }

    fn finish(&mut self) {
        let complete = !matches!(self.framing, Framing::UntilClose);
        self.framing = Framing::Done;
        if let Some(conn) = self.conn.take() {
            // Leftover bytes would be mistaken for the next response.
            if self.keep_alive && complete && conn.buf.is_empty() {
                self.pool.checkin(conn.key, conn.stream);
            }
        }
    }
}

impl Read for ResponseBody {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let Some(conn) = self.conn.as_mut() else {
            return Ok(0);
        };
        let (n, complete) = match &mut self.framing {
            Framing::Done => (0, true),
            Framing::Length(remaining) => {
                if *remaining == 0 {
                    (0, true)
                } else {
                    if conn.buf.is_empty()
                        && conn.buf.read_from(&mut &conn.stream, READ_CHUNK)? == 0
                    {
                        return Err(io::ErrorKind::UnexpectedEof.into());
                    }
                    // Bounded by the buffer length, so it fits in usize.
                    let n = (conn.buf.len() as u64)
                        .min(*remaining)
                        .min(out.len() as u64) as usize;
                    out[..n].copy_from_slice(&conn.buf.as_slice()[..n]);
                    conn.buf.consume(n);
                    *remaining -= n as u64;
                    (n, *remaining == 0)
                }
            }
            Framing::Chunked(body) => {
                let n = body.read(&mut &conn.stream, &mut conn.buf, out)?;
                (n, body.is_done())
            }
            Framing::UntilClose => {
                let n = if conn.buf.is_empty() {
                    (&conn.stream).read(out)?
                } else {
                    let n = conn.buf.len().min(out.len());
                    out[..n].copy_from_slice(&conn.buf.as_slice()[..n]);
                    conn.buf.consume(n);
                    n
                };
                (n, n == 0)
            }
        };
        if complete {
            self.finish();
        }
        Ok(n)
    }
}

impl fmt::Debug for ResponseBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResponseBody")
            .field("complete", &self.is_complete())
            .field("content_length", &self.content_length())
            .finish()
    }
}

/// Maps a body read error: framing errors surface as
/// [`Error::Protocol`], timeouts as [`Error::Timeout`].
pub(crate) fn body_error(err: io::Error) -> Error {
    if err.kind() == io::ErrorKind::InvalidData {
        if let Some(parse) = err
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<ParseError>())
        {
            return match parse {
                ParseError::BodyTooLarge { limit } => Error::BodyTooLarge { limit: *limit },
                other => Error::Protocol(other.clone()),
            };
        }
    }
    Error::from_io(err, "read")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let client = TcpStream::connect(listener.local_addr().expect("addr")).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        (client, server)
    }

    fn key() -> PoolKey {
        ("127.0.0.1".to_owned(), 1)
    }

    #[test]
    fn pool_hands_out_live_connections_lifo() {
        let pool = Pool::new(4, Duration::from_secs(60));
        let (a, _a_peer) = pair();
        let (b, _b_peer) = pair();
        let b_port = b.local_addr().expect("addr").port();
        pool.checkin(key(), a);
        pool.checkin(key(), b);
        assert_eq!(pool.idle_count(), 2);
        let first = pool.checkout(&key()).expect("live connection");
        assert_eq!(first.local_addr().expect("addr").port(), b_port);
        assert_eq!(pool.idle_count(), 1);
    }

    #[test]
    fn pool_discards_connections_closed_by_the_peer() {
        let pool = Pool::new(4, Duration::from_secs(60));
        let (conn, peer) = pair();
        pool.checkin(key(), conn);
        drop(peer);
        // Give the FIN a moment to arrive.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut result = pool.checkout(&key());
        while result.is_some() && Instant::now() < deadline {
            pool.checkin(key(), result.take().expect("checked above"));
            thread::sleep(Duration::from_millis(5));
            result = pool.checkout(&key());
        }
        assert!(result.is_none(), "closed connection must not be reused");
        assert_eq!(pool.idle_count(), 0);
    }

    #[test]
    fn pool_discards_connections_with_unsolicited_data() {
        let pool = Pool::new(4, Duration::from_secs(60));
        let (conn, mut peer) = pair();
        peer.write_all(b"HTTP/1.1 408 Request Timeout\r\n\r\n")
            .expect("write");
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut probe = [0u8; 1];
        while conn.peek(&mut probe).unwrap_or(0) == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        pool.checkin(key(), conn);
        assert!(pool.checkout(&key()).is_none());
    }

    #[test]
    fn pool_respects_limits_and_expiry() {
        let pool = Pool::new(1, Duration::from_secs(60));
        let (a, _pa) = pair();
        let (b, _pb) = pair();
        pool.checkin(key(), a);
        pool.checkin(key(), b);
        assert_eq!(pool.idle_count(), 1);

        let expired = Pool::new(4, Duration::ZERO);
        let (c, _pc) = pair();
        expired.checkin(key(), c);
        assert!(expired.checkout(&key()).is_none());

        let disabled = Pool::new(0, Duration::from_secs(60));
        let (d, _pd) = pair();
        disabled.checkin(key(), d);
        assert_eq!(disabled.idle_count(), 0);
    }

    #[test]
    fn body_errors_are_classified() {
        let chunk = io::Error::new(io::ErrorKind::InvalidData, ParseError::InvalidChunk("x"));
        assert!(matches!(
            body_error(chunk),
            Error::Protocol(ParseError::InvalidChunk("x"))
        ));
        let big = io::Error::new(
            io::ErrorKind::InvalidData,
            ParseError::BodyTooLarge { limit: 3 },
        );
        assert!(matches!(body_error(big), Error::BodyTooLarge { limit: 3 }));
        let eof = io::Error::from(io::ErrorKind::UnexpectedEof);
        assert!(matches!(body_error(eof), Error::ConnectionClosed));
        let slow = io::Error::from(io::ErrorKind::TimedOut);
        assert!(body_error(slow).is_timeout());
    }
}
