//! Server configuration.

use std::fmt;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::method::Method;
use crate::proto::parse::HeadLimits;

/// Problems the server cannot report to a client, passed to
/// [`ServerConfig::error_hook`] so the application can log them.
///
/// Malformed requests are not reported here: they are the client's
/// problem, answered with a 4xx, and counted in
/// [`ServerStats::protocol_errors`](crate::ServerStats::protocol_errors).
#[derive(Debug)]
pub enum ServerError {
    /// `accept` failed with something other than a per-connection error,
    /// typically file-descriptor exhaustion. The server backs off briefly
    /// and keeps accepting.
    Accept(io::Error),
    /// A connection thread could not be spawned; the client got a 503.
    Spawn(io::Error),
    /// A handler or streaming body panicked; the client got a 500 (or, if
    /// the body was already streaming, a closed connection).
    HandlerPanic {
        /// Request method.
        method: Method,
        /// Request path.
        path: String,
        /// The panic message.
        message: String,
    },
    /// A handler produced a response that cannot be sent (an invalid
    /// header, or a 1xx status); the client got a 500 instead.
    InvalidResponse {
        /// Request method.
        method: Method,
        /// Request path.
        path: String,
        /// What was wrong.
        reason: String,
    },
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServerError::Accept(e) => write!(f, "accept failed: {e}"),
            ServerError::Spawn(e) => write!(f, "cannot spawn connection thread: {e}"),
            ServerError::HandlerPanic {
                method,
                path,
                message,
            } => write!(f, "handler for {method} {path} panicked: {message}"),
            ServerError::InvalidResponse {
                method,
                path,
                reason,
            } => write!(f, "invalid response to {method} {path}: {reason}"),
        }
    }
}

impl std::error::Error for ServerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ServerError::Accept(e) | ServerError::Spawn(e) => Some(e),
            _ => None,
        }
    }
}

/// Callback for [`ServerError`]s.
pub type ErrorHook = Arc<dyn Fn(&ServerError) + Send + Sync>;

/// Server limits and timeouts.
///
/// Construct with struct-update syntax over the defaults:
///
/// ```
/// use std::time::Duration;
/// use belay_http::ServerConfig;
///
/// let config = ServerConfig {
///     max_body_bytes: 1 << 20,
///     keep_alive_timeout: Duration::from_secs(30),
///     ..ServerConfig::default()
/// };
/// ```
#[derive(Clone)]
pub struct ServerConfig {
    /// Maximum simultaneous connections, which is also the maximum number
    /// of connection threads. Each connection owns a thread for its whole
    /// life, including idle keep-alive time and long-lived streams, so this
    /// must cover the expected number of long-poll and event-stream clients
    /// plus headroom. Connections beyond it get an immediate `503` with
    /// `Retry-After: 1`. Once more than 7/8 of the slots are in use,
    /// responses stop offering keep-alive so idle connections give their
    /// slots back. Default 1024.
    pub max_connections: usize,
    /// How long an idle keep-alive connection is kept open between
    /// requests. Zero disables keep-alive. Default 5 s.
    pub keep_alive_timeout: Duration,
    /// Time allowed to receive a complete request head, measured from its
    /// first byte (from the accept, for a connection's first request).
    /// Expiry answers `408`. Default 10 s.
    pub header_timeout: Duration,
    /// Longest silence tolerated while receiving a request body; expiry
    /// answers `408`. Default 30 s.
    pub body_timeout: Duration,
    /// Socket write timeout: a client that stops reading its response is
    /// disconnected after this long. Default 30 s.
    pub write_timeout: Duration,
    /// Maximum request-line length; longer ones get `414`. Default 8 KiB.
    pub max_request_line: usize,
    /// Maximum size of the request head (request line plus fields); larger
    /// heads get `431`. Default 32 KiB.
    pub max_head_bytes: usize,
    /// Maximum number of header fields; more get `431`. Default 100.
    pub max_headers: usize,
    /// Maximum request body size; larger bodies get `413`, before any of
    /// the body is read when the length is declared. Default 16 MiB.
    pub max_body_bytes: usize,
    /// Requests served on one connection before it is closed; 0 means no
    /// limit. Default 0.
    pub max_requests_per_connection: usize,
    /// How long a graceful shutdown waits for in-flight requests and
    /// streams before giving up on them. Default 30 s.
    pub shutdown_grace: Duration,
    /// Granularity of the connection loop: idle connections check for
    /// shutdown, and timeouts are enforced, at this interval. Default
    /// 100 ms.
    pub poll_interval: Duration,
    /// How long a pooled thread waits for a new connection before exiting.
    /// Default 60 s.
    pub thread_idle_timeout: Duration,
    /// Value of the `Server` header, if any. Default `belay`.
    pub server_header: Option<String>,
    /// Receives errors that cannot be reported to a client.
    pub error_hook: Option<ErrorHook>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            max_connections: 1024,
            keep_alive_timeout: Duration::from_secs(5),
            header_timeout: Duration::from_secs(10),
            body_timeout: Duration::from_secs(30),
            write_timeout: Duration::from_secs(30),
            max_request_line: 8 * 1024,
            max_head_bytes: 32 * 1024,
            max_headers: 100,
            max_body_bytes: 16 * 1024 * 1024,
            max_requests_per_connection: 0,
            shutdown_grace: Duration::from_secs(30),
            poll_interval: Duration::from_millis(100),
            thread_idle_timeout: Duration::from_secs(60),
            server_header: Some("belay".to_owned()),
            error_hook: None,
        }
    }
}

impl ServerConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        let fail = |msg: String| Err(Error::Config(msg));
        if self.max_connections == 0 {
            return fail("max_connections must be at least 1".into());
        }
        for (name, value) in [
            ("header_timeout", self.header_timeout),
            ("body_timeout", self.body_timeout),
            ("write_timeout", self.write_timeout),
            ("poll_interval", self.poll_interval),
            ("thread_idle_timeout", self.thread_idle_timeout),
        ] {
            if value.is_zero() {
                return fail(format!("{name} must be greater than zero"));
            }
        }
        if self.max_headers == 0 {
            return fail("max_headers must be at least 1".into());
        }
        if self.max_request_line < 16 || self.max_head_bytes < self.max_request_line {
            return fail(format!(
                "max_head_bytes ({}) must be at least max_request_line ({}), which must be at least 16",
                self.max_head_bytes, self.max_request_line
            ));
        }
        Ok(())
    }

    pub(crate) fn head_limits(&self) -> HeadLimits {
        HeadLimits {
            start_line: self.max_request_line,
            head: self.max_head_bytes,
            headers: self.max_headers,
        }
    }

    pub(crate) fn report(&self, error: ServerError) {
        if let Some(hook) = &self.error_hook {
            hook(&error);
        }
    }
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerConfig")
            .field("max_connections", &self.max_connections)
            .field("keep_alive_timeout", &self.keep_alive_timeout)
            .field("header_timeout", &self.header_timeout)
            .field("body_timeout", &self.body_timeout)
            .field("write_timeout", &self.write_timeout)
            .field("max_request_line", &self.max_request_line)
            .field("max_head_bytes", &self.max_head_bytes)
            .field("max_headers", &self.max_headers)
            .field("max_body_bytes", &self.max_body_bytes)
            .field(
                "max_requests_per_connection",
                &self.max_requests_per_connection,
            )
            .field("shutdown_grace", &self.shutdown_grace)
            .field("poll_interval", &self.poll_interval)
            .field("thread_idle_timeout", &self.thread_idle_timeout)
            .field("server_header", &self.server_header)
            .field("error_hook", &self.error_hook.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        assert!(ServerConfig::default().validate().is_ok());
    }

    #[test]
    fn rejects_zero_timeouts_and_limits() {
        let zero_poll = ServerConfig {
            poll_interval: Duration::ZERO,
            ..ServerConfig::default()
        };
        assert_eq!(
            zero_poll.validate().unwrap_err().to_string(),
            "invalid configuration: poll_interval must be greater than zero"
        );
        let no_conns = ServerConfig {
            max_connections: 0,
            ..ServerConfig::default()
        };
        assert!(no_conns.validate().is_err());
        let inverted = ServerConfig {
            max_request_line: 4096,
            max_head_bytes: 1024,
            ..ServerConfig::default()
        };
        assert!(inverted.validate().is_err());
    }

    #[test]
    fn zero_keep_alive_is_allowed() {
        let config = ServerConfig {
            keep_alive_timeout: Duration::ZERO,
            ..ServerConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn debug_hides_the_hook() {
        let config = ServerConfig {
            error_hook: Some(Arc::new(|_: &ServerError| {})),
            ..ServerConfig::default()
        };
        assert!(format!("{config:?}").contains("error_hook: true"));
    }
}
