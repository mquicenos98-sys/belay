//! Access logging through a caller-supplied callback, so the crate stays
//! independent of any particular logger.

use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::handler::{Middleware, Next};
use crate::method::Method;
use crate::middleware::RequestId;
use crate::request::Request;
use crate::response::Response;
use crate::router::MatchedRoute;
use crate::status::StatusCode;

/// One completed request, as passed to the [`AccessLog`] callback.
///
/// `Display` renders it as a logfmt line:
/// `method=GET path=/v1/jobs/7 route=/v1/jobs/:id status=200 duration_ms=0.412 …`
#[derive(Debug)]
pub struct AccessEntry<'a> {
    /// Request method.
    pub method: &'a Method,
    /// Request path (still percent-encoded).
    pub path: &'a str,
    /// The matched route pattern, when a router handled the request.
    pub route: Option<&'a str>,
    /// Response status.
    pub status: StatusCode,
    /// Time spent producing the response. For streaming bodies this ends
    /// when the handler returns, before the stream is written.
    pub duration: Duration,
    /// Peer address.
    pub remote_addr: Option<SocketAddr>,
    /// The request id, when [`SetRequestId`](crate::middleware::SetRequestId)
    /// runs outside this layer.
    pub request_id: Option<&'a str>,
    /// Request body size.
    pub request_bytes: usize,
    /// Response body size, when known up front.
    pub response_bytes: Option<u64>,
}

impl fmt::Display for AccessEntry<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "method={} path=", self.method)?;
        write_value(f, self.path)?;
        if let Some(route) = self.route {
            f.write_str(" route=")?;
            write_value(f, route)?;
        }
        write!(
            f,
            " status={} duration_ms={:.3}",
            self.status.as_u16(),
            self.duration.as_secs_f64() * 1000.0
        )?;
        if let Some(addr) = self.remote_addr {
            write!(f, " remote={addr}")?;
        }
        if let Some(id) = self.request_id {
            f.write_str(" request_id=")?;
            write_value(f, id)?;
        }
        write!(f, " bytes_in={}", self.request_bytes)?;
        if let Some(n) = self.response_bytes {
            write!(f, " bytes_out={n}")?;
        }
        Ok(())
    }
}

/// Quotes values that would otherwise break logfmt tokenization.
fn write_value(f: &mut fmt::Formatter<'_>, value: &str) -> fmt::Result {
    if value.is_empty()
        || value.contains(|c: char| c == ' ' || c == '=' || c == '"' || c.is_control())
    {
        write!(f, "{value:?}")
    } else {
        f.write_str(value)
    }
}

type Sink = Arc<dyn Fn(&AccessEntry<'_>) + Send + Sync>;

/// Middleware that reports every request to a callback after the handler
/// has produced a response.
///
/// ```
/// use std::sync::{Arc, Mutex};
/// use belay_http::middleware::AccessLog;
/// use belay_http::{Handler, Method, Request, Response, Stack};
///
/// let lines = Arc::new(Mutex::new(Vec::new()));
/// let sink = Arc::clone(&lines);
/// let app = Stack::new(|_: &mut Request| Response::text("ok"))
///     .with(AccessLog::new(move |entry| sink.lock().unwrap().push(entry.to_string())));
/// app.call(&mut Request::new(Method::Get, "/healthz"));
/// assert!(lines.lock().unwrap()[0].starts_with("method=GET path=/healthz status=200"));
/// ```
#[derive(Clone)]
pub struct AccessLog {
    sink: Sink,
}

impl AccessLog {
    /// Calls `sink` once per request.
    pub fn new(sink: impl Fn(&AccessEntry<'_>) + Send + Sync + 'static) -> Self {
        AccessLog {
            sink: Arc::new(sink),
        }
    }
}

impl fmt::Debug for AccessLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AccessLog")
    }
}
