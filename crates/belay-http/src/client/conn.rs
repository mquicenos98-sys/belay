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
