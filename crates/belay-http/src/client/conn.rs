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
