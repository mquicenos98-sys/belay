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
